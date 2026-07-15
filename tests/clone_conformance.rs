//! P3 Tier-4 clone(56) v1 conformance gate.
//!
//! v1 supports ONLY the two TID-writeback flag bits
//! (`CLONE_CHILD_SETTID = 0x01000000`, `CLONE_PARENT_SETTID = 0x08000000`).
//! Any other flag bit → `-EINVAL` (`-22`). The handler allocates a new
//! PID from `Kernel.next_pid` (starting at 2) and writes it to the
//! requested `*_tidptr` locations. The child is **not** actually
//! executed in v1 — that's deferred per the implementation plan.
//!
//! Fixture WAT writes the syscall args + flag bit selection into
//! linear memory, then calls `(import "kernel" "syscall")` with the
//! composed arguments. The host then reads the `*_tidptr` slots to
//! verify the writeback semantics.

use anyhow::Result;

mod common;

const FIXTURE_WAT: &str = r#"
    (module
      (import "kernel" "syscall"
        (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
      (memory (export "memory") 1)
      ;; Layout (4-byte aligned slots):
      ;;   0x100: flags (i32)         — set by host before _start.
      ;;   0x104: ptid slot (i32)     — host reads back here.
      ;;   0x108: ctid slot (i32)     — host reads back here.
      ;;   0x10c: expected_pid (i32)  — host records the return value.
      (func (export "_start") (result i64)
        (local $flags i32)
        (local $ptid_ptr i32)
        (local $ctid_ptr i32)
        (local.set $flags (i32.load (i32.const 0x100)))
        (local.set $ptid_ptr (i32.load (i32.const 0x110)))
        (local.set $ctid_ptr (i32.load (i32.const 0x114)))
        ;; flags=0 means "no supported flags" — call clone(0) which
        ;; must return -EINVAL (-22).
        (if (i32.eqz (local.get $flags))
          (then
            (i32.store (i32.const 0x120)
              (i32.wrap_i64
                (call $syscall
                  (i64.const 56)        ;; NR_CLONE
                  (i64.const 0)         ;; flags=0
                  (i64.const 0)         ;; child_stack
                  (i64.const 0)         ;; ptid_ptr=NULL
                  (i64.const 0)         ;; ctid_ptr=NULL
                  (i64.const 0) (i64.const 0))))
            (return (i64.const 0))))
        ;; Otherwise call with the requested flags and write the new
        ;; PID to 0x120. The host reads 0x120 + ptid/ctid slots to
        ;; verify writeback semantics.
        (i32.store (i32.const 0x120)
          (i32.wrap_i64
            (call $syscall
              (i64.const 56)
              (i64.extend_i32_s (local.get $flags))
              (i64.const 0)              ;; child_stack
              (i64.extend_i32_u (local.get $ptid_ptr))
              (i64.extend_i32_u (local.get $ctid_ptr))
              (i64.const 0) (i64.const 0))))
        (i64.const 0)))
"#;

/// Instantiate the fixture with a fresh kernel, attach memory,
/// return the (store, instance) pair. Memory is NOT pre-populated —
/// the caller writes flags/ptid/ctid pointers via `mem_write_*`.
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

fn mem_write_i32(store: &mut wasmtime::Store<edge_libos::Kernel>, offset: usize, val: i32) {
    let mem = *store.data().memory().expect("memory attached");
    let bytes = val.to_ne_bytes();
    mem.write(store, offset, &bytes).unwrap();
}

fn mem_read_i32(store: &wasmtime::Store<edge_libos::Kernel>, offset: usize) -> i32 {
    let mem = *store.data().memory().expect("memory attached");
    let mut buf = [0u8; 4];
    mem.read(store, offset, &mut buf).unwrap();
    i32::from_ne_bytes(buf)
}

/// Helper: call `_start` once and return the guest's trap-or-return i64.
async fn call_start(
    store: &mut wasmtime::Store<edge_libos::Kernel>,
    instance: &wasmtime::Instance,
) -> i64 {
    let start = instance
        .get_typed_func::<(), i64>(&mut *store, "_start")
        .expect("_start export");
    start.call_async(&mut *store, ()).await.unwrap_or(0)
}

#[tokio::test]
async fn clone_no_supported_flags_returns_einval() -> Result<()> {
    let (mut store, instance) = fresh_store_with_fixture().await?;
    mem_write_i32(&mut store, 0x100, 0); // flags = 0
    call_start(&mut store, &instance).await;
    let ret = mem_read_i32(&store, 0x120);
    assert_eq!(ret, -22, "clone(0) must return -EINVAL (-22)");
    Ok(())
}

#[tokio::test]
async fn clone_unsupported_flag_returns_einval() -> Result<()> {
    let (mut store, instance) = fresh_store_with_fixture().await?;
    // CLONE_FILES (0x400) is in v1's reject set and remains rejected
    // under v2 per ADR 0005 §6. Pairing it with a TID-writeback flag
    // exercises the "any bit outside CLONE_SUPPORTED_V2 → -EINVAL"
    // contract — the supported flag (CLONE_CHILD_SETTID) does NOT
    // mask the unsupported one.
    mem_write_i32(&mut store, 0x100, 0x400 | 0x0100_0000);
    mem_write_i32(&mut store, 0x110, 0x200); // ptid_ptr
    mem_write_i32(&mut store, 0x114, 0x204); // ctid_ptr
    call_start(&mut store, &instance).await;
    let ret = mem_read_i32(&store, 0x120);
    assert_eq!(
        ret, -22,
        "clone(CLONE_FILES | CLONE_CHILD_SETTID) must return -EINVAL"
    );
    Ok(())
}

#[tokio::test]
async fn clone_vm_thread_flags_accepted_and_writes_tid() -> Result<()> {
    // M4: `clone(CLONE_VM | CLONE_THREAD | CLONE_CHILD_SETTID |
    // CLONE_PARENT_SETTID)` is accepted at the flag-validation layer.
    // The full SharedMemory hand-off lands in M7; for M4 we only
    // assert that the flag set is NOT rejected and that TID
    // writeback works.
    let (mut store, instance) = fresh_store_with_fixture().await?;
    let flags = 0x100_i32 | 0x10000_i32 | 0x0100_0000 | 0x0800_0000;
    mem_write_i32(&mut store, 0x100, flags);
    mem_write_i32(&mut store, 0x110, 0x200); // ptid_ptr
    mem_write_i32(&mut store, 0x114, 0x204); // ctid_ptr
    mem_write_i32(&mut store, 0x200, 0xdead_beef_u32 as i32);
    mem_write_i32(&mut store, 0x204, 0xcafe_babe_u32 as i32);
    call_start(&mut store, &instance).await;
    let child_pid = mem_read_i32(&store, 0x120);
    assert!(
        child_pid > 1,
        "clone(CLONE_VM | CLONE_THREAD | …) must return child_pid > 1, got {child_pid}"
    );
    let ptid = mem_read_i32(&store, 0x200);
    let ctid = mem_read_i32(&store, 0x204);
    assert_eq!(ptid, child_pid, "CLONE_PARENT_SETTID must match child_pid");
    assert_eq!(ctid, child_pid, "CLONE_CHILD_SETTID must match child_pid");
    Ok(())
}

#[tokio::test]
async fn clone_vm_without_tid_writeback_returns_einval() -> Result<()> {
    // M4: a pure CLONE_VM | CLONE_THREAD (no TID-writeback flag)
    // is rejected — the guest has no way to observe the child TID.
    // This matches the v1 "clone(0) == -EINVAL" conformance rule.
    let (mut store, instance) = fresh_store_with_fixture().await?;
    mem_write_i32(&mut store, 0x100, 0x100 | 0x10000);
    call_start(&mut store, &instance).await;
    let ret = mem_read_i32(&store, 0x120);
    assert_eq!(
        ret, -22,
        "clone(CLONE_VM | CLONE_THREAD) without TID-writeback must return -EINVAL"
    );
    Ok(())
}

#[tokio::test]
async fn clone_supported_flags_writes_parent_and_child_tid() -> Result<()> {
    let (mut store, instance) = fresh_store_with_fixture().await?;
    mem_write_i32(&mut store, 0x100, 0x0100_0000 | 0x0800_0000);
    mem_write_i32(&mut store, 0x110, 0x200); // ptid_ptr
    mem_write_i32(&mut store, 0x114, 0x204); // ctid_ptr
                                             // Pre-write sentinels so we can detect "the kernel wrote here".
    mem_write_i32(&mut store, 0x200, 0xdead_beef_u32 as i32);
    mem_write_i32(&mut store, 0x204, 0xcafe_babe_u32 as i32);
    call_start(&mut store, &instance).await;
    let child_pid = mem_read_i32(&store, 0x120);
    assert!(
        child_pid > 1,
        "clone() must return child_pid > 1 (got {child_pid})"
    );
    let ptid = mem_read_i32(&store, 0x200);
    let ctid = mem_read_i32(&store, 0x204);
    assert_eq!(
        ptid, child_pid,
        "parent_tidptr must be written with child_pid"
    );
    assert_eq!(
        ctid, child_pid,
        "child_tidptr must be written with child_pid"
    );
    Ok(())
}

#[tokio::test]
async fn clone_only_parent_settid_writes_only_parent() -> Result<()> {
    let (mut store, instance) = fresh_store_with_fixture().await?;
    mem_write_i32(&mut store, 0x100, 0x0800_0000); // CLONE_PARENT_SETTID only
    mem_write_i32(&mut store, 0x110, 0x200);
    mem_write_i32(&mut store, 0x114, 0x204);
    mem_write_i32(&mut store, 0x200, 0x1111_2222_i32);
    mem_write_i32(&mut store, 0x204, 0x3333_4444_i32);
    // sentinel values (signed casts; only used as "did kernel write").
    call_start(&mut store, &instance).await;
    let child_pid = mem_read_i32(&store, 0x120);
    assert!(child_pid > 1);
    assert_eq!(mem_read_i32(&store, 0x200), child_pid, "ptid written");
    assert_eq!(
        mem_read_i32(&store, 0x204),
        0x3333_4444_i32,
        "ctid untouched (no CLONE_CHILD_SETTID)"
    );
    Ok(())
}

#[tokio::test]
async fn clone_invalid_ptid_pointer_returns_efault() -> Result<()> {
    let (mut store, instance) = fresh_store_with_fixture().await?;
    mem_write_i32(&mut store, 0x100, 0x0800_0000);
    // 0xFFFFFFFC is out of 1-page (64 KiB) linear memory.
    mem_write_i32(&mut store, 0x110, -4_i32);
    mem_write_i32(&mut store, 0x114, 0x200);
    call_start(&mut store, &instance).await;
    let ret = mem_read_i32(&store, 0x120);
    assert_eq!(ret, -14, "bad ptid_ptr must return -EFAULT (-14)");
    Ok(())
}
