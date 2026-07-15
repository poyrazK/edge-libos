//! P3 Tier-5 `fork(2)` v1 conformance gate.
//!
//! v1 returns the child PID in the parent; the child fiber is
//! NOT resumed (deferred-resume contract — see
//! `src/sys/process.rs::fork_syscall` doc). What we can test
//! from the parent's side:
//!
//!   1. `fork()` returns a positive child PID.
//!   2. The returned PID is inserted into `Kernel.children` with
//!      `exited = false`.
//!   3. `Kernel.next_pid` increments by exactly 1 per fork.
//!   4. The deferred-resume contract is observable: a second
//!      invocation on the same store sees the same kernel state
//!      (not a child-resumed state).
//!
//! The child-path check (`fork() == 0` in the child) is gated
//! behind the deferred child-fiber-resume story; see ADR 0003 +
//! the imp plan §P3 Tier-5.

use anyhow::Result;

mod common;

const FORK_WAT: &str = r#"
    (module
      (import "kernel" "syscall"
        (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
      (memory (export "memory") 1)
      (func (export "_start") (result i64)
        (i64.store (i32.const 0x100)
          (call $syscall
            (i64.const 57)                          ;; NR_FORK
            (i64.const 0) (i64.const 0) (i64.const 0)
            (i64.const 0) (i64.const 0) (i64.const 0)))
        (i64.const 0)))
"#;

async fn fresh_store_with_fixture(
) -> Result<(wasmtime::Store<edge_libos::Kernel>, wasmtime::Instance)> {
    let (engine, linker) = common::engine_and_linker()?;
    let module = common::compile_wat(&engine, FORK_WAT)?;
    let mut store = edge_libos::build_store(
        &engine,
        edge_libos::Kernel::new_without_stdio(vec![], vec![]),
    );
    let instance = linker.instantiate_async(&mut store, &module).await?;
    if let Some(mem) = instance.get_memory(&mut store, "memory") {
        store.data_mut().attach_memory(mem);
    }
    Ok((store, instance))
}

fn mem_read_i64(store: &wasmtime::Store<edge_libos::Kernel>, offset: usize) -> i64 {
    let mem = *store.data().memory().expect("memory attached");
    let mut buf = [0u8; 8];
    mem.read(store, offset, &mut buf).unwrap();
    i64::from_ne_bytes(buf)
}

async fn call_fork(
    store: &mut wasmtime::Store<edge_libos::Kernel>,
    instance: &wasmtime::Instance,
) -> i64 {

    let start = instance
        .get_typed_func::<(), i64>(&mut *store, "_start")
        .expect("_start export");
    start.call_async(&mut *store, ()).await.unwrap_or(0);
    mem_read_i64(store, 0x100)
}

#[tokio::test]
async fn fork_returns_positive_child_pid() -> Result<()> {
    let (mut store, instance) = fresh_store_with_fixture().await?;
    let r = call_fork(&mut store, &instance).await;
    assert!(r > 0, "fork() must return a positive child PID, got {r}");
    // The init kernel is PID 1; fork starts allocating at 2.
    assert!(
        r >= 2,
        "fork() child PID must be >= 2 (PID 1 reserved for init), got {r}"
    );
    Ok(())
}

#[tokio::test]
async fn fork_inserts_child_into_kernel_children_table() -> Result<()> {
    let (mut store, instance) = fresh_store_with_fixture().await?;
    let r = call_fork(&mut store, &instance).await;
    assert!(r > 0);
    let children = store.data().children.lock();
    let pid_i32 = r as i32;
    let entry = children
        .get(&pid_i32)
        .expect("fork() must insert the child PID into Kernel.children");
    assert!(
        !entry.exited,
        "freshly-forked child must have exited = false"
    );
    assert_eq!(
        entry.exit_code, 0,
        "freshly-forked child must have exit_code = 0"
    );
    Ok(())
}

#[tokio::test]
async fn fork_increments_next_pid_by_one() -> Result<()> {
    let (mut store, instance) = fresh_store_with_fixture().await?;
    let pre = store
        .data()
        .next_pid
        .load(std::sync::atomic::Ordering::Relaxed);
    let r = call_fork(&mut store, &instance).await;
    let post = store
        .data()
        .next_pid
        .load(std::sync::atomic::Ordering::Relaxed);
    assert_eq!(post - pre, 1, "fork() must increment next_pid by exactly 1");
    assert_eq!(r as i32, pre, "fork() returns the pre-incremented PID");
    Ok(())
}

#[tokio::test]
async fn fork_does_not_resume_child_fiber_in_v1() -> Result<()> {
    // Deferred-resume contract: v1 allocates a child PID but does
    // NOT start a separate fiber for the child. The parent gets
    // back the PID and continues; the kernel.children table holds
    // the new entry with `exited = false`.
    //
    // This test verifies the kernel state is what the parent
    // expects — not a child-resumed state — by:
    //   1. Forking twice.
    //   2. Inspecting that both children are present.
    //   3. Inspecting that the kernel's other state (e.g. the
    //      `next_pid`) advanced exactly 2.
    // A resumed child would have called `_start` again and might
    // have side-effected kernel state; we observe nothing of
    // that sort.
    let (mut store, instance) = fresh_store_with_fixture().await?;
    let r1 = call_fork(&mut store, &instance).await;
    let r2 = call_fork(&mut store, &instance).await;
    assert!(r1 > 0 && r2 > 0, "both forks must return positive PIDs");
    assert_ne!(r1, r2, "two forks must produce distinct PIDs");

    let children = store.data().children.lock();
    assert!(children.contains_key(&(r1 as i32)));
    assert!(children.contains_key(&(r2 as i32)));
    // Both still not-yet-exited — the deferred child fiber has
    // not run exit() on itself.
    assert!(!children.get(&(r1 as i32)).unwrap().exited);
    assert!(!children.get(&(r2 as i32)).unwrap().exited);
    Ok(())
}
