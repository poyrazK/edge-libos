//! Shared test harness for integration tests.
//!
//! Most P0 tests need: an Engine, a Linker with our dispatch registered, and
//! a tiny WAT module that imports `kernel.syscall`. This module provides the
//! common scaffolding so individual tests stay focused on the syscall under
//! test.

use std::path::Path;

use anyhow::Result;
use wasmtime::{Engine, Instance, Linker, Module, Store};

use edge_libos::{add_to_linker, build_engine, build_store, Kernel};

/// Build a fresh Engine + Linker pre-registered with the dispatch.
// Shared helper: each test target compiles this module independently, and
// a test binary that doesn't reach every helper trips `dead_code`. Allow
// per-fn rather than module-wide so genuine dead code in callers still fires.
#[allow(dead_code)]
pub fn engine_and_linker() -> Result<(Engine, Linker<Kernel>)> {
    let engine = build_engine()?;
    let mut linker = Linker::new(&engine);
    add_to_linker(&mut linker)?;
    Ok((engine, linker))
}

/// Compile a WAT string into a Module on the given engine.
#[allow(dead_code)]
pub fn compile_wat(engine: &Engine, wat: &str) -> Result<Module> {
    let bytes = wat::parse_str(wat)?;
    Ok(Module::new(engine, &bytes)?)
}

/// Instantiate a Module, attach its memory to the Kernel, and return the
/// Store + Instance. The Kernel is freshly constructed with empty args/env.
///
/// **Async note:** wasmtime 45.0.3 has `Config::async_support` always on,
/// so `Linker::instantiate` becomes `instantiate_async`. The harness is
/// async; callers wrap with `tokio::runtime::Runtime::block_on`.
#[allow(dead_code)]
pub async fn instantiate_async(
    engine: &Engine,
    linker: &Linker<Kernel>,
    module: &Module,
) -> Result<(Store<Kernel>, Instance)> {
    let mut store = build_store(engine, Kernel::new(vec![], vec![]));
    let instance = linker.instantiate_async(&mut store, module).await?;
    if let Some(mem) = instance.get_memory(&mut store, "memory") {
        store.data_mut().attach_memory(mem);
    }
    Ok((store, instance))
}

/// Build a Kernel rooted at `preopen`. Use this for VFS tests that need to
/// read/write real files on disk.
#[allow(dead_code)]
pub fn kernel_with_preopen(preopen: impl AsRef<Path>) -> Kernel {
    Kernel::new_with_preopen(vec![], vec![], preopen.as_ref())
}
