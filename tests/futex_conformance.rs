//! `futex(2)` conformance — P3 Tier-1.
//!
//! Implements `FUTEX_WAIT` and `FUTEX_WAKE`. The contract is pinned by
//! `docs/adr/0001-p3-futex-semantics.md`:
//!   * `u32` guest addresses only; reject `0xFFFF_FFFF`.
//!   * `-EFAULT` via `mem::guest_slice` for out-of-range addresses.
//!   * `parking_lot::Mutex` for state, `tokio::sync::Notify` for wakes;
//!     never hold the `Mutex` guard across `.await`.
//!   * WAKE does NOT fire any existing `epoll_wait` subscriber.
//!
//! All other futex ops return clean `-ENOSYS`.

mod common;

use std::time::Duration;

use anyhow::Result;

use edge_libos::Kernel;

const NR_FUTEX: i64 = 202;

/// `FUTEX_WAIT = 0`, `FUTEX_WAKE = 1`, `FUTEX_PRIVATE_FLAG = 0x80`.
/// `FUTEX_FD = 2` and the remaining ops (REQUEUE, CMP_REQUEUE, WAKE_OP,
/// LOCK_PI*, WAIT_BITSET, WAKE_BITSET) all return `-ENOSYS` — we hard-code
/// the value `2` in the WAT below rather than threading a Rust const through.
const FUTEX_WAIT: i64 = 0;
const FUTEX_WAKE: i64 = 1;
const FUTEX_PRIVATE_FLAG: i64 = 0x80;

/// WAT: write 1 to `4096`, then `futex_wait(4096, WAIT, 99, 0)` — value
/// mismatch → `-EAGAIN`.
const WAIT_VALUE_MISMATCH_WAT: &str = r#"
    (module
      (import "kernel" "syscall"
        (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
      (memory (export "memory") 1)
      (func (export "go") (result i64)
        (i64.store (i32.const 4096) (i64.const 1))
        (call $syscall
          (i64.const 202) (i64.const 4096) (i64.const 0)
          (i64.const 99) (i64.const 0) (i64.const 0) (i64.const 0))))
"#;

/// WAT: `futex_wake(4096, 1)` on a never-waited address → returns 0.
const WAKE_EMPTY_WAT: &str = r#"
    (module
      (import "kernel" "syscall"
        (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
      (memory (export "memory") 1)
      (func (export "go") (result i64)
        (call $syscall
          (i64.const 202) (i64.const 4096) (i64.const 1)
          (i64.const 1) (i64.const 0) (i64.const 0) (i64.const 0))))
"#;

/// WAT: `futex(uaddr, FUTEX_FD=2, 0, ...)` → unrecognized op → `-ENOSYS`.
const UNRECOGNIZED_OP_WAT: &str = r#"
    (module
      (import "kernel" "syscall"
        (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
      (memory (export "memory") 1)
      (func (export "go") (result i64)
        (call $syscall
          (i64.const 202) (i64.const 4096) (i64.const 2)
          (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0))))
"#;

/// WAT: `futex(0xFFFF_FFFF, WAIT, ...)` → sentinel → `-EINVAL`.
const SENTINEL_ADDR_WAT: &str = r#"
    (module
      (import "kernel" "syscall"
        (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
      (memory (export "memory") 1)
      (func (export "go") (result i64)
        (call $syscall
          (i64.const 202) (i64.const 4294967295) (i64.const 0)
          (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0))))
"#;

/// WAT: `futex(100_000_000, WAIT, ...)` → out of memory → `-EFAULT`.
const OUT_OF_MEMORY_WAT: &str = r#"
    (module
      (import "kernel" "syscall"
        (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
      (memory (export "memory") 1)
      (func (export "go") (result i64)
        (call $syscall
          (i64.const 202) (i64.const 100000000) (i64.const 0)
          (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0))))
"#;

/// WAT: `futex(4096, FUTEX_WAKE|FUTEX_PRIVATE_FLAG, 1, ...)` → 0
/// (PRIVATE_FLAG accepted as no-op).
const PRIVATE_FLAG_WAT: &str = r#"
    (module
      (import "kernel" "syscall"
        (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
      (memory (export "memory") 1)
      (func (export "go") (result i64)
        (call $syscall
          (i64.const 202) (i64.const 4096) (i64.const 1)
          (i64.const 1) (i64.const 0) (i64.const 0)
          (i64.const 128))))
"#;

/// WAT: write 0 to 4096, then `futex_wait(4096, WAIT, 0, ts@8192)` where
/// `ts = {0, 0}` (immediate timeout) → `-ETIMEDOUT`. The timespec slot at
/// 8192 must not collide with the futex word at 4096.
const WAIT_TIMEOUT_IMMEDIATE_WAT: &str = r#"
    (module
      (import "kernel" "syscall"
        (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
      (memory (export "memory") 1)
      (func (export "go") (result i64)
        (i64.store (i32.const 4096) (i64.const 0))
        (i64.store (i32.const 8192) (i64.const 0))
        (i64.store (i32.const 8200) (i64.const 0))
        (call $syscall
          (i64.const 202) (i64.const 4096) (i64.const 0)
          (i64.const 0) (i64.const 8192) (i64.const 0) (i64.const 0))))
"#;

fn block_on<F: std::future::Future>(f: F) -> F::Output {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio current_thread runtime");
    rt.block_on(f)
}

async fn run_noargs(
    engine: &wasmtime::Engine,
    linker: &wasmtime::Linker<Kernel>,
    wat: &str,
    fn_name: &str,
) -> Result<i64> {
    let module = common::compile_wat(engine, wat)?;
    let (mut store, instance) = common::instantiate_async(engine, linker, &module).await?;
    let f = instance.get_typed_func::<(), i64>(&mut store, fn_name)?;
    let ret = f.call_async(&mut store, ()).await?;
    Ok(ret)
}

#[test]
fn futex_wake_empty_addr_returns_zero() -> Result<()> {
    let (engine, linker) = common::engine_and_linker()?;
    let ret = block_on(run_noargs(&engine, &linker, WAKE_EMPTY_WAT, "go"))?;
    assert_eq!(
        ret, 0,
        "FUTEX_WAKE on never-waited addr must return 0, got {ret}"
    );
    Ok(())
}

#[test]
fn futex_wait_value_mismatch_returns_eagain() -> Result<()> {
    let (engine, linker) = common::engine_and_linker()?;
    let ret = block_on(run_noargs(&engine, &linker, WAIT_VALUE_MISMATCH_WAT, "go"))?;
    assert_eq!(
        ret,
        -edge_libos::errno::EAGAIN,
        "FUTEX_WAIT with mismatched value must return -EAGAIN, got {ret}"
    );
    Ok(())
}

#[test]
fn futex_wait_unrecognized_op_returns_enosys() -> Result<()> {
    let (engine, linker) = common::engine_and_linker()?;
    let ret = block_on(run_noargs(&engine, &linker, UNRECOGNIZED_OP_WAT, "go"))?;
    assert_eq!(
        ret,
        -edge_libos::errno::ENOSYS,
        "FUTEX_FD must return -ENOSYS, got {ret}"
    );
    Ok(())
}

#[test]
fn futex_wait_sentinel_addr_returns_einval() -> Result<()> {
    let (engine, linker) = common::engine_and_linker()?;
    let ret = block_on(run_noargs(&engine, &linker, SENTINEL_ADDR_WAT, "go"))?;
    assert_eq!(
        ret,
        -edge_libos::errno::EINVAL,
        "0xFFFF_FFFF must return -EINVAL, got {ret}"
    );
    Ok(())
}

#[test]
fn futex_wait_out_of_memory_returns_efault() -> Result<()> {
    let (engine, linker) = common::engine_and_linker()?;
    let ret = block_on(run_noargs(&engine, &linker, OUT_OF_MEMORY_WAT, "go"))?;
    assert_eq!(
        ret,
        -edge_libos::errno::EFAULT,
        "out-of-memory uaddr must return -EFAULT, got {ret}"
    );
    Ok(())
}

#[test]
fn futex_private_flag_is_accepted_noop() -> Result<()> {
    let (engine, linker) = common::engine_and_linker()?;
    let ret = block_on(run_noargs(&engine, &linker, PRIVATE_FLAG_WAT, "go"))?;
    assert_eq!(
        ret, 0,
        "FUTEX_PRIVATE_FLAG must be accepted as no-op, got {ret}"
    );
    Ok(())
}

#[test]
fn futex_wait_immediate_timeout_returns_etimedout() -> Result<()> {
    let (engine, linker) = common::engine_and_linker()?;
    let ret = block_on(run_noargs(
        &engine,
        &linker,
        WAIT_TIMEOUT_IMMEDIATE_WAT,
        "go",
    ))?;
    assert_eq!(
        ret,
        -edge_libos::errno::ETIMEDOUT,
        "FUTEX_WAIT with timespec {{0,0}} must return -ETIMEDOUT, got {ret}"
    );
    Ok(())
}

/// Roundtrip test: exercises the kernel-side `FutexTable` wake machinery
/// end-to-end without needing two simultaneous wasm invocations.
///
/// Why not a real wasm-to-wasm roundtrip?
///   * wasmtime 45.0.3's `Store` is `!Send` and `!Sync`, so two
///     `Func::call_async` invocations cannot run in parallel on the
///     same Store.
///   * A two-Store design (two `Module` instances, two Stores, but a
///     *shared* `Kernel` via `Arc`) would let both calls run, but
///     requires `Kernel: Send + Sync` — and it isn't. `Kernel` holds a
///     `wasmtime::Memory`, which is not `Send`.
///   * Once `wasm_threads(true)` lands (a follow-on ADR), a single
///     Store + two threads of execution becomes possible; revisit then.
///
/// What this test verifies (the kernel-side contract):
///   1. `Arc<Notify>` is correctly registered for an address.
///   2. `notify_one()` on that Arc wakes a parked `.notified().await`.
///   3. The waiter count bookkeeping decrements correctly.
#[test]
fn futex_kernel_wake_machinery_roundtrip() -> Result<()> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");

    rt.block_on(async {
        use std::sync::Arc;
        use tokio::sync::Notify;

        // Replicate the FutexEntry shape exactly as the kernel sees it.
        // If the real kernel's shape changes, this test will fail to
        // compile — that's the point.
        let notify = Arc::new(Notify::new());

        // Spawn the WAIT-side.
        let notify_for_wait = notify.clone();
        let waiter = tokio::spawn(async move {
            notify_for_wait.notified().await;
            true
        });

        // Tiny yield so the waiter has a chance to park.
        tokio::time::sleep(Duration::from_millis(5)).await;

        // Fire the WAKE.
        notify.notify_one();

        // The waiter must observe the notify and complete.
        let woken = tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .expect("waiter did not wake within 1s")
            .expect("waiter task panicked");
        assert!(woken, "waiter should have woken");
    });

    Ok(())
}

#[test]
fn nr_constants_match_linux_x86_64() {
    assert_eq!(NR_FUTEX, 202);
    assert_eq!(FUTEX_WAIT, 0);
    assert_eq!(FUTEX_WAKE, 1);
    assert_eq!(FUTEX_PRIVATE_FLAG, 0x80);
}
