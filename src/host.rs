//! Wasmtime Engine / Store / Linker factory.
//!
//! `build_engine` is the single place that defines the Wasmtime feature set we
//! support. P0 enables: async host functions, the component model, reference
//! types, and SIMD. P3 Tier-3 also enables: `wasm_threads`,
//! `shared_memory`, and `wasm_shared_everything_threads` — see ADR 0001 §2
//! and the in-source comment on `build_engine` for the rationale.

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
    //
    // P3 Tier-3 — see ADR 0001 §2. All three independent gates flipped
    // together because they unlock cross-fiber wakeups as a unit:
    //   * `wasm_threads(true)` — threads proposal parser/validator
    //     (atomics + the `shared` flag on memory declarations).
    //   * `shared_memory(true)` — runtime `SharedMemory::new(...)`,
    //     required for instantiating modules that declare
    //     `(memory ... shared)`. Without this, the parser allows
    //     `(memory ... shared)` but instantiation still fails.
    //   * `wasm_shared_everything_threads(true)` — the tier-3
    //     `thread.spawn` guest instruction. Required for the eventual
    //     `clone(56)` handler to spawn real guest fibers.
    // There is no auto-enabling logic in wasmtime 45.0.3 — these three
    // are independent bits. `Cargo.toml:22` adds `"threads"` to the
    // wasmtime feature list to enable `wasmtime-cranelift?/threads` and
    // `wasmtime-winch?/threads` compile-feature bits; no new transitive
    // crates. `Store` is still `!Send`/`!Sync` — each fiber pins to its
    // host thread; cross-fiber wakeups go through shared-memory atomics
    // on a `SharedMemory`.
    cfg.wasm_threads(true);
    cfg.shared_memory(true);
    cfg.wasm_shared_everything_threads(true);
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
