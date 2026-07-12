//! Wasmtime Engine / Store / Linker factory.
//!
//! `build_engine` is the single place that defines the Wasmtime feature set we
//! support. P0 enables: async host functions, the component model, reference
//! types, and SIMD. Threads are disabled (single-threaded v1, see
//! `impelementationplan` §1.4).

use anyhow::Result;
use wasmtime::{Config, Engine, Linker, Store};

use crate::dispatch;
use crate::kernel::Kernel;

/// Build a Wasmtime [`Engine`] configured for the edge-libos guest ABI.
pub fn build_engine() -> Result<Engine> {
    let mut cfg = Config::new();
    cfg.wasm_component_model(true);
    cfg.wasm_reference_types(true);
    cfg.wasm_simd(true);
    // NB: in wasmtime 45.0.3, async host functions are always supported —
    // `Config::async_support` is deprecated and a no-op.
    cfg.wasm_threads(false); // v1 single-threaded
    Ok(Engine::new(&cfg)?)
}

/// Build a fresh [`Store`] rooted at the given [`Kernel`].
pub fn build_store(engine: &Engine, kernel: Kernel) -> Store<Kernel> {
    Store::new(engine, kernel)
}

/// Register the `kernel.syscall` import with the linker.
///
/// Call this exactly once per linker, before instantiating any module.
pub fn add_to_linker(linker: &mut Linker<Kernel>) -> Result<()> {
    dispatch::register(linker)
}
