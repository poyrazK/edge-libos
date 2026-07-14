//! P2-D3.5: shared helpers for `edge-cli` subcommand modules.
//!
//! Currently home to `call_start`, which invokes the guest's `_start`
//! export with the right signature (musl/zig uses `() -> ()`; emscripten
//! uses `() -> i32`). Used by `freeze` (to drive the guest as a
//! background task while the foreground polls for a materialized
//! listener) and by `serve` (to respawn the guest after `apply_snapshot`).

use wasmtime::{Instance, Store};

use crate::cli::error::{CliError, CliResult};
use crate::kernel::Kernel;

/// Call the guest's `_start` export. Tries `() -> ()` first; falls back
/// to `() -> i32` for emscripten-built modules. Returns `CliError::Args`
/// if neither signature is present.
///
/// The dual-signature try is intentional: `cargo run --bin edge-cli -- run`
/// must accept both musl zig cc builds (the common path for our guests)
/// and emscripten builds (used by some third-party wasm).
pub async fn call_start(instance: &Instance, store: &mut Store<Kernel>) -> CliResult<()> {
    if let Ok(start) = instance.get_typed_func::<(), ()>(&mut *store, "_start") {
        start
            .call_async(&mut *store, ())
            .await
            .map_err(CliError::from)
            .map(|_| ())
    } else if let Ok(start) = instance.get_typed_func::<(), i32>(&mut *store, "_start") {
        start
            .call_async(&mut *store, ())
            .await
            .map_err(CliError::from)
            .map(|_| ())
    } else {
        Err(CliError::Args(
            "no _start export (expected () -> () or () -> i32)".to_string(),
        ))
    }
}
