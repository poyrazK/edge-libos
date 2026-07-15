//! Wasmtime Engine / Store / Linker factory.
//!
//! `build_engine` is the single place that defines the Wasmtime feature set we
//! support. P0 enables: async host functions, the component model, reference
//! types, and SIMD. P3 Tier-3 also enables: `wasm_threads`,
//! `shared_memory`, and `wasm_shared_everything_threads` — see ADR 0001 §2
//! and the in-source comment on `build_engine` for the rationale.
//!
//! P2 metering (ADR 0004): `consume_fuel(true)` is flipped
//! unconditionally so every Store can have a per-request fuel
//! budget. Subcommands call `Store::set_fuel(ms_to_fuel(budget))` at
//! the per-request entry point — see `src/cli/{run,serve,bench}.rs`.
//! The yield interval is deliberately left OFF (see the body of
//! `build_store`) — see ADR 0004 §1 for the empirical rationale.

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
    // ADR 0004 §1: enable fuel metering at engine build. Required
    // for `Store::set_fuel` to function. Discovered empirically
    // that wasmtime 45.0.3 instruments each wasm instruction with
    // a fuel check; with `fuel_async_yield_interval` ALSO set, the
    // fiber re-enters the wasm at the host-call site and double-
    // invokes the host function (observed in
    // `tests/snapshot_roundtrip.rs`). We therefore enable fuel at
    // the engine level but deliberately do NOT call
    // `fuel_async_yield_interval` in `build_store` — see ADR 0004
    // §1 "what this ADR blocks".
    cfg.consume_fuel(true);
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
    //     the declaration but instantiation rejects it.
    //   * `wasm_shared_everything_threads(true)` — the
    //     `wasm_shared_everything_threads` proposal, which extends
    //     the threads proposal to allow `mut` globals and tables
    //     shared across stores. Required for our kernel since a
    //     guest fiber may be hosted in a different `Store` than
    //     the kernel's per-`Store` `Kernel` struct (a Store is
    //     pinned to a host thread; cross-fiber wakeups go through
    //     shared-memory atomics on a `SharedMemory`).
    cfg.wasm_threads(true);
    cfg.shared_memory(true);
    cfg.wasm_shared_everything_threads(true);
    Ok(Engine::new(&cfg)?)
}

/// Build a fresh [`Store`] rooted at the given [`Kernel`].
///
/// ADR 0004 §1: every Store is built with fuel **enabled** (via
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
/// safety — see ADR 0004 §1 "what this ADR blocks".
pub fn build_store(engine: &Engine, kernel: Kernel) -> Store<Kernel> {
    let mut store = Store::new(engine, kernel);
    store
        .set_fuel(u64::MAX)
        .expect("build_store: fuel is enabled at engine build; set_fuel should always succeed");
    store
}

/// Register the `kernel.syscall` import with the linker.
///
/// Call this exactly once per linker, before instantiating any module.
pub fn add_to_linker(linker: &mut Linker<Kernel>) -> Result<()> {
    dispatch::register(linker)
}

// ---------------------------------------------------------------------------
// P3 Tier-8 v2 step 1 — child-thread helpers
// ---------------------------------------------------------------------------
//
// `Store<Kernel>` is `!Send + !Sync` per wasmtime 45.0.3, and so is
// `Linker<Kernel>`. The parent thread that handles a fork()/clone()
// cannot share its Store/Linker with the spawned child fiber (which
// runs on a brand-new `std::thread`). Each child must therefore build
// its own Store + Linker. The helpers below isolate that pattern so
// the call site in `src/sys/process.rs::spawn_child_thread` reads
// cleanly.
//
// `Engine` and `Module` are `Send + Sync` (per wasmtime docs), so the
// parent wraps them in `Arc` and clones the Arc into the child thread
// — that's the entire reason fork_syscall takes `Arc<Engine>` +
// `Arc<Module>` instead of `&Engine` / `&Module`.
///
/// Build a fresh child-thread `Linker<Kernel>`. The linker is
/// `!Send + !Sync` and must be constructed on the thread that will
/// own it. Cost is one `func_new_async` registration per syscall —
/// negligible (~tens of µs).
pub fn build_child_linker(engine: &wasmtime::Engine) -> Result<Linker<Kernel>> {
    let mut linker: Linker<Kernel> = Linker::new(engine);
    add_to_linker(&mut linker)?;
    Ok(linker)
}

/// Build a fresh child-thread `Store<Kernel>`. Same body as
/// `build_store` (pre-fills `u64::MAX` fuel per ADR 0004 §1) but
/// documented separately so the caller contract is explicit: the
/// kernel belongs to a forked/cloned child fiber, not the parent.
pub fn build_child_store(engine: &wasmtime::Engine, kernel: Kernel) -> Store<Kernel> {
    build_store(engine, kernel)
}
