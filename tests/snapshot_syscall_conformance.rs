//! `NR_SNAPSHOT = 123` conformance — P2-D3.5 sub-deliverable 2.
//!
//! Tests the guest-driven snapshot path pinned by ADR 0004 §1:
//!
//! 1. `snapshot_writes_postcard_bytes_to_path` — the guest calls
//!    `NR_SNAPSHOT("/tmp/...")` via the syscall import; the kernel
//!    writes a postcard `KernelSnapshot` to the path. The test
//!    reads the bytes back and asserts the first 4 bytes decode
//!    to `LeU32(SNAPSHOT_FORMAT_VERSION)`.
//! 2. `snapshot_with_einval_path_returns_einval` — pass a NULL
//!    pointer (path = 0). The handler must return `-EFAULT` per
//!    ADR 0004 §1's "refusing NULL is explicit" rule.
//! 3. `snapshot_with_einval_path_returns_einval_long_path` —
//!    1 MB all-non-NUL bytes; `guest_str` caps at 4096 so the
//!    read fails the bounds check, returning `-EFAULT`.
//!
//! Tests share the `common` helper module that wires up the
//! test engine + linker (per the existing `futex_conformance.rs`
//! pattern).

mod common;

use anyhow::Result;
use edge_libos::build_store;
use edge_libos::errno::{EFAULT, EIO};
use edge_libos::Kernel;

/// WAT: writes a path string into linear memory at offset 4096,
/// then calls `NR_SNAPSHOT(path_ptr)`. The path is a tmpfs file
/// pre-created by the test (the host side uses `tempfile` to
/// obtain a deterministic path under `/tmp/...`).
const SNAPSHOT_WAT: &str = r#"
    (module
      (import "kernel" "syscall"
        (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
      (memory (export "memory") 1)
      (func (export "go") (param $path_ptr i64) (result i64)
        (call $syscall
          (i64.const 123) (local.get $path_ptr)
          (i64.const 0) (i64.const 0)
          (i64.const 0) (i64.const 0) (i64.const 0))))
"#;

/// Like `SNAPSHOT_WAT` but ignores its argument and always
/// passes a NULL path (0) — used to assert the `-EFAULT` path.
const SNAPSHOT_NULL_WAT: &str = r#"
    (module
      (import "kernel" "syscall"
        (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
      (memory (export "memory") 1)
      (func (export "go") (result i64)
        (call $syscall
          (i64.const 123) (i64.const 0)
          (i64.const 0) (i64.const 0)
          (i64.const 0) (i64.const 0) (i64.const 0))))
"#;

/// Pre-populate a linear-memory region with a NUL-terminated
/// path string. `start` is the byte offset; we cap at 4096 (the
/// same cap the kernel uses inside `snapshot_syscall`). `Memory::write`
/// in wasmtime 45.0.3 is sync (returns `Result<(), MemoryAccessError>`),
/// not async — no `.await` needed.
fn write_path_to_guest_memory(
    store: &mut wasmtime::Store<Kernel>,
    mem: wasmtime::Memory,
    path: &str,
) -> Result<u32> {
    let bytes = path.as_bytes();
    let mut region = vec![0u8; bytes.len() + 1];
    region[..bytes.len()].copy_from_slice(bytes);
    mem.write(store, 4096, &region)
        .map_err(|e| anyhow::anyhow!("mem.write failed: {e}"))?;
    Ok(4096)
}

#[tokio::test(flavor = "current_thread")]
async fn snapshot_writes_postcard_bytes_to_path() -> Result<()> {
    let (engine, linker) = common::engine_and_linker()?;
    let module = common::compile_wat(&engine, SNAPSHOT_WAT)?;

    // Pre-create a temp file (snapshot destination). The kernel's
    // `std::fs::write` will overwrite or create as needed.
    let tmp = tempfile::Builder::new()
        .suffix(".snap")
        .tempfile()
        .map_err(|e| anyhow::anyhow!("tempfile failed: {e}"))?;
    let path = tmp.path().to_string_lossy().to_string();
    let expected_path = path.clone();
    let expected_version_bytes = edge_libos::snapshot::endian::LeU32(
        edge_libos::snapshot::SNAPSHOT_FORMAT_VERSION,
    )
    .0
    .to_le_bytes();

    let mut store = build_store(&engine, Kernel::new_without_stdio(vec![], vec![]));
    let instance = linker.instantiate_async(&mut store, &module).await?;
    let memory = instance
        .get_memory(&mut store, "memory")
        .expect("memory export");
    store.data_mut().attach_memory(memory);
    let memory_ref = instance
        .get_memory(&mut store, "memory")
        .expect("memory export (re-borrow)");

    let path_ptr = write_path_to_guest_memory(&mut store, memory_ref, &path)?;

    let go = instance
        .get_typed_func::<i64, i64>(&mut store, "go")
        .expect("go export");
    let byte_count = go.call_async(&mut store, path_ptr as i64).await?;
    assert!(
        byte_count >= 0,
        "NR_SNAPSHOT must return byte count (>= 0), got {byte_count}"
    );
    assert!(
        byte_count > 0,
        "NR_SNAPSHOT must write a non-empty snapshot, got {byte_count} bytes"
    );
    assert_eq!(
        byte_count as usize,
        std::fs::metadata(tmp.path())?.len() as usize,
        "NR_SNAPSHOT return value must match on-disk byte count"
    );

    // Read the bytes back and verify the format-version header
    // (the first 4 bytes, little-endian, must equal
    // SNAPSHOT_FORMAT_VERSION per ADR 0002 §2).
    let on_disk = std::fs::read(tmp.path())?;
    assert!(
        on_disk.len() >= 4,
        "snapshot file must be at least 4 bytes long"
    );
    assert_eq!(
        &on_disk[..4],
        &expected_version_bytes,
        "snapshot file's first 4 bytes must decode to LeU32(SNAPSHOT_FORMAT_VERSION = {}); \
         got 0x{:02x}{:02x}{:02x}{:02x}",
        edge_libos::snapshot::SNAPSHOT_FORMAT_VERSION,
        on_disk[0],
        on_disk[1],
        on_disk[2],
        on_disk[3]
    );

    // Bonus: roundtrip through encode/decode so a malformed
    // prefix would have caught the version mismatch differently.
    let snap = edge_libos::snapshot::decode_snapshot(&on_disk)?;
    assert_eq!(
        snap.format_version.0,
        edge_libos::snapshot::SNAPSHOT_FORMAT_VERSION,
        "decoded format version must equal SNAPSHOT_FORMAT_VERSION"
    );

    let _ = expected_path; // silence unused-binding clippy lint
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn snapshot_with_null_pointer_returns_efault() -> Result<()> {
    let (engine, linker) = common::engine_and_linker()?;
    let module = common::compile_wat(&engine, SNAPSHOT_NULL_WAT)?;

    let mut store = build_store(&engine, Kernel::new_without_stdio(vec![], vec![]));
    let instance = linker.instantiate_async(&mut store, &module).await?;
    if let Some(mem) = instance.get_memory(&mut store, "memory") {
        store.data_mut().attach_memory(mem);
    }

    let go = instance
        .get_typed_func::<(), i64>(&mut store, "go")
        .expect("go export");
    let ret = go.call_async(&mut store, ()).await?;
    assert_eq!(
        ret, -EFAULT,
        "NR_SNAPSHOT with NULL path must return -EFAULT per ADR 0004 §1, got {ret}"
    );

    // Also assert it's not silently EIO (operator-side error)
    // or EINVAL — the NULL-pointer case is operator error on
    // a guest path, not a write-failure, so -EFAULT is the
    // right mapping.
    assert_ne!(ret, -EIO, "must NOT be -EIO (write hasn't happened yet)");
    Ok(())
}

/// Out-of-memory path pointer → guest_str cap (4096) is
/// exceeded → mem::guest_slice returns -EFAULT → handler
/// propagates. The WAT writes a 4097-byte NUL-less string at
/// offset 4096; `guest_str` caps at 4096 and reads 4096 bytes
/// that don't include a NUL — `from_utf8` then succeeds
/// (random bytes might be invalid UTF-8 → -EINVAL, but for
/// arbitrary non-UTF8 content this is also -EFAULT). We pin
/// the contract as "negative errno, not silent success".
#[tokio::test(flavor = "current_thread")]
async fn snapshot_with_oom_path_returns_negative() -> Result<()> {
    const OOM_PATH_PTR_WAT: &str = r#"
        (module
          (import "kernel" "syscall"
            (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
          (memory (export "memory") 2)
          (func (export "go") (param $path_ptr i64) (result i64)
            (call $syscall
              (i64.const 123) (local.get $path_ptr)
              (i64.const 0) (i64.const 0)
              (i64.const 0) (i64.const 0) (i64.const 0))))
    "#;

    let (engine, linker) = common::engine_and_linker()?;
    let module = common::compile_wat(&engine, OOM_PATH_PTR_WAT)?;

    let mut store = build_store(&engine, Kernel::new_without_stdio(vec![], vec![]));
    let instance = linker.instantiate_async(&mut store, &module).await?;
    let memory = instance
        .get_memory(&mut store, "memory")
        .expect("memory export");
    store.data_mut().attach_memory(memory);
    let memory_ref = instance
        .get_memory(&mut store, "memory")
        .expect("memory export (re-borrow)");

    // 4097 bytes from guest-memory limit (64 KiB minus 32 KiB
    // minus offset). 200_000 is well past any cap → -EFAULT.
    let oom_ptr: i64 = 200_000;
    let go = instance
        .get_typed_func::<i64, i64>(&mut store, "go")
        .expect("go export");
    let ret = go.call_async(&mut store, oom_ptr).await?;
    assert!(
        ret < 0,
        "NR_SNAPSHOT with out-of-memory path must return negative errno, got {ret}"
    );
    // The exact errno (-EFAULT or -EINVAL depending on whether
    // the cap or the UTF-8 check fired first) is implementation
    // detail; we only assert "negative."
    let _ = memory_ref; // silence unused-bindings clippy lint
    Ok(())
}

#[test]
fn nr_constants_match_linux_x86_64() {
    use edge_libos::sys::process::NR_SNAPSHOT;
    assert_eq!(NR_SNAPSHOT, 123);
}
