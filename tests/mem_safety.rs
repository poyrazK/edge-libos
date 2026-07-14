//! `mem.rs` EFAULT safety tests.
//!
//! Per spec §8: "fuzz the pointer/len args of every syscall for EFAULT-safety;
//! a host that segfaults on a bad guest pointer is a sandbox escape, not a
//! bug." This is the smallest slice: for each (NR, arg-pattern) that touches
//! a pointer+len, we expect the host to return `-EFAULT` and stay alive.
//!
//! The full per-syscall EFAULT fuzzer (one test per (NR, poison) combo)
//! lands in Step 17 of the build order. This file exercises the four
//! canonical poison classes against NR_WRITE.

mod common;

use anyhow::Result;

use edge_libos::sys::file::NR_WRITE;
use edge_libos::Kernel;

/// WAT module that exposes a `call` function: takes nr + 6 i64 args, calls
/// `kernel.syscall(nr, a1..a6)`, returns the host's result. One instance,
/// many call sites.
const CALLER_WAT: &str = r#"
    (module
      (import "kernel" "syscall"
        (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
      (memory (export "memory") 1)
      (func (export "call")
        (param $nr i64) (param $a1 i64) (param $a2 i64)
        (param $a3 i64) (param $a4 i64) (param $a5 i64) (param $a6 i64)
        (result i64)
        (call $syscall (local.get $nr) (local.get $a1) (local.get $a2)
                       (local.get $a3) (local.get $a4) (local.get $a5)
                       (local.get $a6))
      )
    )
"#;

fn block_on<F: std::future::Future>(f: F) -> F::Output {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio current_thread runtime");
    rt.block_on(f)
}

async fn dispatch_argv(
    engine: &wasmtime::Engine,
    linker: &wasmtime::Linker<Kernel>,
    module: &wasmtime::Module,
    nr: i64,
    a: [i64; 6],
) -> Result<i64> {
    let (mut store, instance) = common::instantiate_async(engine, linker, module).await?;
    let f =
        instance.get_typed_func::<(i64, i64, i64, i64, i64, i64, i64), i64>(&mut store, "call")?;
    let ret = f
        .call_async(&mut store, (nr, a[0], a[1], a[2], a[3], a[4], a[5]))
        .await?;
    Ok(ret)
}

#[test]
fn eisrange_constants() {
    // Sanity: the constants are what we say they are.
    assert_eq!(edge_libos::errno::EFAULT, 14);
    assert_eq!(edge_libos::errno::to_ret(14), -14);
}

#[test]
fn dispatch_survives_negative_pointer_on_write() -> Result<()> {
    let (engine, linker) = common::engine_and_linker()?;
    let module = common::compile_wat(&engine, CALLER_WAT)?;
    let ret = block_on(dispatch_argv(
        &engine,
        &linker,
        &module,
        NR_WRITE as i64,
        [1, -1, 10, 0, 0, 0],
    ))?;
    assert_eq!(
        ret,
        -edge_libos::errno::EFAULT,
        "negative pointer must yield -EFAULT, got {ret}"
    );
    Ok(())
}

#[test]
fn dispatch_survives_pointer_past_end_of_memory() -> Result<()> {
    // 1-page memory = 64 KiB. A pointer well past that with a non-zero len
    // must return -EFAULT, not crash.
    let (engine, linker) = common::engine_and_linker()?;
    let module = common::compile_wat(&engine, CALLER_WAT)?;
    let ret = block_on(dispatch_argv(
        &engine,
        &linker,
        &module,
        NR_WRITE as i64,
        [1, 100_000, 10, 0, 0, 0],
    ))?;
    assert_eq!(
        ret,
        -edge_libos::errno::EFAULT,
        "pointer past end of memory must yield -EFAULT, got {ret}"
    );
    Ok(())
}

#[test]
fn dispatch_survives_negative_len_on_write() -> Result<()> {
    let (engine, linker) = common::engine_and_linker()?;
    let module = common::compile_wat(&engine, CALLER_WAT)?;
    let ret = block_on(dispatch_argv(
        &engine,
        &linker,
        &module,
        NR_WRITE as i64,
        [1, 0, -1, 0, 0, 0],
    ))?;
    assert_eq!(
        ret,
        -edge_libos::errno::EFAULT,
        "negative length must yield -EFAULT, got {ret}"
    );
    Ok(())
}

#[test]
fn dispatch_survives_overflowing_ptr_plus_len() -> Result<()> {
    // ptr = i64::MAX/2, len = i64::MAX. Inside `guest_slice` we compute
    // p.checked_add(l) and bail on overflow with -EFAULT.
    let (engine, linker) = common::engine_and_linker()?;
    let module = common::compile_wat(&engine, CALLER_WAT)?;
    let huge = i64::MAX / 2;
    let ret = block_on(dispatch_argv(
        &engine,
        &linker,
        &module,
        NR_WRITE as i64,
        [1, huge, huge, 0, 0, 0],
    ))?;
    assert_eq!(
        ret,
        -edge_libos::errno::EFAULT,
        "ptr+len overflow must yield -EFAULT, got {ret}"
    );
    Ok(())
}

#[test]
fn memory_attached_has_expected_size() -> Result<()> {
    // Sanity: a 1-page wasm memory is 64 KiB. This is the baseline the
    // other tests rely on.
    const WAT: &str = r#"
        (module
          (import "kernel" "syscall"
            (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
          (memory (export "memory") 1)
        )
    "#;
    let (engine, linker) = common::engine_and_linker()?;
    let module = common::compile_wat(&engine, WAT)?;
    block_on(async {
        let (store, _instance) = common::instantiate_async(&engine, &linker, &module).await?;
        let mem = store.data().memory.as_ref().expect("memory attached");
        let size = mem.data(&store).len();
        assert_eq!(size, 65536, "1-page wasm memory must be 64 KiB");
        Ok::<(), anyhow::Error>(())
    })
}
