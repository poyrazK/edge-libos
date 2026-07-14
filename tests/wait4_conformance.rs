//! P3 Tier-6 wait4(61) v1 conformance gate.
//!
//! v1 honors only `WNOHANG`. A future PR (gated on PR 4's child
//! fiber that can actually trigger `exit` on behalf of a child)
//! adds the parked-wait path. The kernel exposes
//! `Kernel.children: Mutex<HashMap<i32, ChildExitStatus>>` and
//! `Kernel.child_event: Arc<Notify>` so the follow-up can land
//! without an additional breaking field change.
//!
//! Fixture WAT writes the syscall args into linear memory, calls
//! `(import "kernel" "syscall")` with the composed arguments, and
//! stores the i64 return value at a known offset the host then reads.

use anyhow::Result;

mod common;

const FIXTURE_WAT: &str = r#"
    (module
      (import "kernel" "syscall"
        (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
      (memory (export "memory") 1)
      ;; Layout (8-byte slots):
      ;;   0x100: arg0 (i64) — pid
      ;;   0x108: arg1 (i64) — wstatus ptr (0 for NULL)
      ;;   0x110: arg2 (i64) — options
      ;;   0x118: arg3 (i64) — rusage (always 0 in v1)
      ;;   0x120: return value (i64)
      (func (export "_start") (result i64)
        (i64.store (i32.const 0x120)
          (call $syscall
            (i64.const 61)                          ;; NR_WAIT4
            (i64.load (i32.const 0x100))             ;; pid
            (i64.load (i32.const 0x108))             ;; wstatus
            (i64.load (i32.const 0x110))             ;; options
            (i64.load (i32.const 0x118))             ;; rusage
            (i64.const 0) (i64.const 0)))
        (i64.const 0)))
"#;

async fn fresh_store_with_fixture(
) -> Result<(wasmtime::Store<edge_libos::Kernel>, wasmtime::Instance)> {
    let (engine, linker) = common::engine_and_linker()?;
    let module = common::compile_wat(&engine, FIXTURE_WAT)?;
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

fn mem_write_i64(store: &mut wasmtime::Store<edge_libos::Kernel>, offset: usize, val: i64) {
    let mem = *store.data().memory().expect("memory attached");
    mem.write(store, offset, &val.to_ne_bytes()).unwrap();
}

fn mem_read_i64(store: &wasmtime::Store<edge_libos::Kernel>, offset: usize) -> i64 {
    let mem = *store.data().memory().expect("memory attached");
    let mut buf = [0u8; 8];
    mem.read(store, offset, &mut buf).unwrap();
    i64::from_ne_bytes(buf)
}

async fn call_wait4(
    store: &mut wasmtime::Store<edge_libos::Kernel>,
    instance: &wasmtime::Instance,
    pid: i64,
    wstatus: i64,
    options: i64,
) -> i64 {
    mem_write_i64(store, 0x100, pid);
    mem_write_i64(store, 0x108, wstatus);
    mem_write_i64(store, 0x110, options);
    mem_write_i64(store, 0x118, 0);
    let start = instance
        .get_typed_func::<(), i64>(&mut *store, "_start")
        .expect("_start export");
    start.call_async(&mut *store, ()).await.unwrap_or(0);
    mem_read_i64(store, 0x120)
}

#[tokio::test]
async fn wait4_no_children_returns_echild() -> Result<()> {
    let (mut store, instance) = fresh_store_with_fixture().await?;
    // options = 0 (no WNOHANG). With no children, v1 returns -ECHILD.
    let r = call_wait4(&mut store, &instance, -1, 0, 0).await;
    assert_eq!(
        r, -10,
        "wait4(-1, 0, 0) with no children must return -ECHILD (-10)"
    );
    Ok(())
}

#[tokio::test]
async fn wait4_wnohang_no_children_returns_echild() -> Result<()> {
    let (mut store, instance) = fresh_store_with_fixture().await?;
    // Linux: wait4(any, WNOHANG) with no children → -ECHILD
    // (WNOHANG is irrelevant when there is nothing to wait for).
    let r = call_wait4(&mut store, &instance, -1, 0, 0x40).await;
    assert_eq!(
        r, -10,
        "wait4(any, WNOHANG) with no children must return -ECHILD"
    );
    Ok(())
}

#[tokio::test]
async fn wait4_wnohang_nonexistent_pid_returns_echild() -> Result<()> {
    let (mut store, instance) = fresh_store_with_fixture().await?;
    let r = call_wait4(&mut store, &instance, 99999, 0, 0x40).await;
    assert_eq!(r, -10, "wait4(99999, WNOHANG) must return -ECHILD");
    Ok(())
}

#[tokio::test]
async fn wait4_unsupported_flag_returns_einval() -> Result<()> {
    let (mut store, instance) = fresh_store_with_fixture().await?;
    // WUNTRACED (0x02) is not in the v1 supported mask.
    let r = call_wait4(&mut store, &instance, -1, 0, 0x02).await;
    assert_eq!(r, -22, "wait4(WUNTRACED) must return -EINVAL");
    Ok(())
}

#[tokio::test]
async fn wait4_process_group_pid_returns_einval() -> Result<()> {
    let (mut store, instance) = fresh_store_with_fixture().await?;
    // pid < -1 selects a process group — not supported in v1.
    let r = call_wait4(&mut store, &instance, -2, 0, 0x40).await;
    assert_eq!(r, -22, "wait4(-2, WNOHANG) must return -EINVAL");
    Ok(())
}

#[tokio::test]
async fn wait4_wnohang_with_populated_child_returns_zero() -> Result<()> {
    let (mut store, instance) = fresh_store_with_fixture().await?;
    // Pre-populate a not-yet-exited child into Kernel.children. WNOHANG
    // must return 0 (no child is ready).
    {
        let mut children = store.data().children.lock();
        children.insert(
            42,
            edge_libos::kernel::ChildExitStatus {
                exit_code: 0,
                exited: false,
            },
        );
    }
    let r = call_wait4(&mut store, &instance, 42, 0, 0x40).await;
    assert_eq!(
        r, 0,
        "wait4(42, WNOHANG) with non-exited child must return 0"
    );
    // The entry must still be present — we only reaped, never popped.
    let children = store.data().children.lock();
    assert!(children.contains_key(&42));
    Ok(())
}

#[tokio::test]
async fn wait4_wnohang_with_reaped_child_returns_pid_and_writes_wstatus() -> Result<()> {
    let (mut store, instance) = fresh_store_with_fixture().await?;
    // Pre-populate an exited child with exit code = 7.
    {
        let mut children = store.data().children.lock();
        children.insert(
            42,
            edge_libos::kernel::ChildExitStatus {
                exit_code: 7,
                exited: true,
            },
        );
    }
    // Allocate a wstatus slot in guest memory at offset 0x200.
    mem_write_i64(&mut store, 0x200, 0); // pre-zero the slot
    let r = call_wait4(&mut store, &instance, 42, 0x200, 0x40).await;
    assert_eq!(r, 42, "wait4(42, WNOHANG) with reaped child must return 42");
    // wstatus should be encoded as (exit_code << 8) = 0x0700.
    let wstatus = mem_read_i64(&store, 0x200);
    assert_eq!(wstatus, 0x0700, "wait status must be (exit_code << 8)");
    // The child must have been popped from the table.
    let children = store.data().children.lock();
    assert!(!children.contains_key(&42));
    Ok(())
}

#[tokio::test]
async fn wait4_any_pid_picks_first_reaped_child() -> Result<()> {
    let (mut store, instance) = fresh_store_with_fixture().await?;
    {
        let mut children = store.data().children.lock();
        children.insert(
            10,
            edge_libos::kernel::ChildExitStatus {
                exit_code: 3,
                exited: true,
            },
        );
        children.insert(
            11,
            edge_libos::kernel::ChildExitStatus {
                exit_code: 0,
                exited: false,
            },
        );
        children.insert(
            12,
            edge_libos::kernel::ChildExitStatus {
                exit_code: 5,
                exited: true,
            },
        );
    }
    let r = call_wait4(&mut store, &instance, -1, 0, 0x40).await;
    let r_i32 = r as i32;
    // HashMap iteration order is not stable across runs — accept any
    // exited PID (10 or 12), and verify the picked one was popped.
    assert!(
        r_i32 == 10 || r_i32 == 12,
        "wait4(any, WNOHANG) with two exited children must pick one (got {r})"
    );
    // The picked PID is removed; the other exited child + the
    // not-yet-exited child remain.
    let children = store.data().children.lock();
    assert!(
        !children.contains_key(&r_i32),
        "reaped child must be popped"
    );
    assert!(children.contains_key(&11), "non-exited child must stay");
    // Exactly one of {10, 12} remains (the one we didn't reap).
    let still_present: usize = [10_i32, 12]
        .iter()
        .filter(|p| children.contains_key(p))
        .count();
    assert_eq!(still_present, 1, "exactly one of {{10,12}} stays");
    Ok(())
}
