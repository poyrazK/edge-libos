//! Wasmtime Engine / Store / Linker factory.
//!
//! `build_engine` is the single place that defines the Wasmtime feature set we
//! support. P0 enables: async host functions, the component model, reference
//! types, and SIMD. Threads are disabled (single-threaded v1, see
//! `impelementationplan` §1.4).
//!
//! P2 metering (ADR 0003): `consume_fuel(true)` is flipped
//! unconditionally. Every Store built by [`build_store`] configures
//! `fuel_async_yield_interval(Some(YIELD_INTERVAL_FUEL))` so that
//! long-running wasm calls periodically yield back to the host
//! runtime. Subcommands call `Store::set_fuel(ms_to_fuel(budget))`
//! at the per-request entry point — see `src/cli/{run,serve,bench}.rs`.

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
    // ADR 0003 §1: enable fuel metering at engine build. Required
    // for `Store::set_fuel` to function. Discovered empirically
    // that wasmtime 45.0.3 instruments each wasm instruction with
    // a fuel check; with `fuel_async_yield_interval` ALSO set, the
    // fiber re-enters the wasm at the host-call site and double-
    // invokes the host function (observed in
    // `tests/snapshot_roundtrip.rs`). We therefore enable fuel at
    // the engine level but deliberately do NOT call
    // `fuel_async_yield_interval` in `build_store` — see ADR 0003
    // §1 "what this ADR blocks".
    cfg.consume_fuel(true);
    // NB: in wasmtime 45.0.3, async host functions are always supported —
    // `Config::async_support` is deprecated and a no-op.
    cfg.wasm_threads(false); // v1 single-threaded — see ADR 0001 §2.
                             // P3 follow-on (wasm_threads(true)): ALSO add "threads" to the
                             // wasmtime feature list in Cargo.toml:22 (currently
                             // ["component-model", "async", "anyhow"]). The bool flip alone is
                             // not enough — wasmtime 45.0.3 gates the threads feature at the
                             // crate-feature level.
    Ok(Engine::new(&cfg)?)
}

/// Build a fresh [`Store`] rooted at the given [`Kernel`].
///
/// ADR 0003 §1: every Store is built with fuel **enabled** (via
/// engine config) and **pre-filled to `u64::MAX`** so that callers
/// who don't care about metering get the pre-ADR behavior
/// (unbounded execution). The wasmtime docs say a Store starts
/// with 0 fuel by default ("it will immediately trap") — without
/// this `set_fuel(u64::MAX)` every existing test would fail at
/// instruction zero. Subcommands that want a real budget call
/// `store.set_fuel(ms_to_fuel(budget))` immediately after this
/// returns; the cli::run / cli::serve / cli::bench modules do that.
///
/// We deliberately do NOT call `fuel_async_yield_interval`: with
/// fuel enabled, wasmtime instruments each wasm instruction; with
/// a yield interval set, the fiber re-enters the wasm at the host
/// call site and double-invokes the host handler (observed in
/// `tests/snapshot_roundtrip.rs`). The fix is to keep the yield
/// interval off until every host handler is audited for yield
/// safety — see ADR 0003 §1 "what this ADR blocks".
pub fn build_store(engine: &Engine, kernel: Kernel) -> Store<Kernel> {
    let mut store = Store::new(engine, kernel);
    store.set_fuel(u64::MAX).expect(
        "build_store: fuel is enabled at engine build; set_fuel should always succeed",
    );
    store
}

/// Register the `kernel.syscall` import with the linker.
///
/// Call this exactly once per linker, before instantiating any module.
pub fn add_to_linker(linker: &mut Linker<Kernel>) -> Result<()> {
    dispatch::register(linker)
}
