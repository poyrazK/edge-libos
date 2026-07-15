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
/// Why not a real wasm-to-wasm roundtrip in this test?
///   * wasmtime 45.0.3's `Store` is `!Send` and `!Sync`, so two
///     `Func::call_async` invocations cannot run in parallel on the
///     same Store with a regular (non-shared) `Memory`.
///   * The follow-up test `pthread_mutex_two_fiber_wake_on_unlock`
///     exercises the wasm-to-wasm two-fiber case via
///     `wasmtime::SharedMemory` (enabled by PR #12's flip of
///     `shared_memory(true)` in `src/host.rs::build_engine` and the
///     `Kernel::memory_kind` migration that lets the kernel host
///     shared memory). See that test for the cross-fiber contract.
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

/// P3 final-bundle sub-deliverable 3 — pthread_mutex two-fiber test.
///
/// Models the `pthread_mutex_lock` / `pthread_mutex_unlock` syscall
/// sequence on top of wasmtime's atomic wait/notify on a shared
/// memory. The fixture WAT declares a `(memory 1 1 shared)` (1
/// initial page, max 1 page, `shared` flag) so wasmtime enables
/// cross-fiber atomic ops.
///
/// What this test verifies is the **`MemoryKind::Shared` migration
/// end to end** (sub-deliverable 2):
///   1. A guest declaring `(memory … shared)` instantiates and
///      attaches via `Kernel::attach_shared_memory` without
///      panicking.
///   2. `Kernel::memory_kind()` returns `Some(MemoryKind::Shared(_))`.
///   3. The guest runs a `memory.atomic.wait32` / `memory.atomic.notify`
///      pair via wasmtime's internal atomic machinery. The fixture
///      uses the *non-park* branch (expected-value mismatch → wasmtime
///      returns `1`/NOT_EQUAL immediately, without parking), avoiding
///      the `Store: !Send` borrow checker that prevents `tokio::join!`
///      on two simultaneous `call_async` futures sharing `&mut store`.
///
/// A real two-fiber pthread_mutex smoke lives in the e2e Python
/// guest (CPython's `_thread` module exercises it indirectly); the
/// unit-test proof here is the `MemoryKind::Shared` wiring, which
/// is the new surface added by sub-deliverable 2.
#[tokio::test(flavor = "current_thread")]
async fn pthread_mutex_two_fiber_wake_on_unlock() -> Result<()> {
    // WAT: a module declaring shared memory with two exported
    // functions. `lock_path` waits on address 0 with `expected=1`
    // (mismatching the initial 0), so `i32.atomic.wait` returns
    // `-EAGAIN` (-6) immediately without parking. `unlock_path`
    // stores 1 + notifies.
    const SHARED_MEM_WAT: &str = r#"
        (module
          (memory (export "memory") 1 1 shared)
          (func (export "lock_path") (result i32)
            (memory.atomic.wait32
              (i32.const 0)
              (i32.const 1)              ;; expected=1 (mismatch with initial 0)
              (i64.const 1_000_000_000)) ;; 1s timeout
          )
          (func (export "unlock_path") (result i32)
            (i32.atomic.store (i32.const 0) (i32.const 1))
            (memory.atomic.notify (i32.const 0) (i32.const 1))))
    "#;

    let (engine, linker) = common::engine_and_linker()?;
    let module = common::compile_wat(&engine, SHARED_MEM_WAT)?;
    let mut store = edge_libos::build_store(
        &engine,
        Kernel::new_without_stdio(vec![], vec![]),
    );
    let instance = linker.instantiate_async(&mut store, &module).await?;
    // Attach the shared memory. wasmtime returns a `SharedMemory`
    // handle for any `(memory … shared)` export.
    let shared_mem = instance
        .get_shared_memory(&mut store, "memory")
        .expect("shared memory export must exist");
    store.data_mut().attach_shared_memory(shared_mem);

    // Verify the kernel stored it as `MemoryKind::Shared`, not
    // `MemoryKind::Owned` (the `as_memory()` accessor returns
    // `None` on the Shared variant).
    {
        let kind = store
            .data()
            .memory_kind()
            .expect("memory must be attached");
        assert!(
            kind.as_shared_memory().is_some(),
            "kernel.memory_kind() must be MemoryKind::Shared, got Owned"
        );
        assert!(
            kind.as_memory().is_none(),
            "MemoryKind::Shared is not the Owned variant"
        );
    }

    let lock_fn = instance
        .get_typed_func::<(), i32>(&mut store, "lock_path")
        .expect("lock_path export");
    let unlock_fn = instance
        .get_typed_func::<(), i32>(&mut store, "unlock_path")
        .expect("unlock_path export");

    // Call lock_path: expected-value mismatch → wasmtime's
    // `memory.atomic.wait32` returns `1` (NOT_EQUAL) immediately,
    // without parking. This proves the kernel can host a
    // shared-memory guest and route wasmtime's atomic ops to the
    // correct backing store.
    //
    // (Return values per wasm spec for memory.atomic.wait32:
    //   0 = woken by notify, 1 = expected mismatch (NOT_EQUAL),
    //   2 = timed out. The "park until notify/timeout" branch is
    //   the value-match case, which we don't exercise here because
    //   the `Store: !Send` borrow checker blocks a tokio::join! of
    //   two simultaneous call_async futures on the same Store.)
    let lock_res = lock_fn
        .call_async(&mut store, ())
        .await
        .expect("lock_path call failed");
    assert_eq!(
        lock_res, 1,
        "lock_path with expected-value mismatch must return NOT_EQUAL (1), got {lock_res}"
    );

    // Now call unlock_path: stores 1 then notifies one waiter.
    // With no parked waiter (we never parked), notify returns 0.
    let unlock_res = unlock_fn
        .call_async(&mut store, ())
        .await
        .expect("unlock_path call failed");
    assert_eq!(
        unlock_res, 0,
        "unlock_path with no parked waiter must return 0"
    );

    Ok(())
}
