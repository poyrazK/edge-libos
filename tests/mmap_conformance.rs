//! mmap / munmap round-trip conformance tests.
//!
//! These exercise the `LinearAllocator` end-to-end: guest allocates an
//! anonymous page, writes a known byte pattern into it via wasmtime-mapped
//! `i32.store`, reads it back via `i32.load`, then frees the range with
//! `munmap` and confirms a follow-up `mmap` returns a usable address.
//!
//! All syscalls are issued through the same `(import "kernel" "syscall")`
//! trampoline the dispatch layer exports — so this is the same code path
//! real CPython will hit when it asks musl for heap pages.

mod common;

use anyhow::Result;
use wasmtime::Store;

use edge_libos::errno;
use edge_libos::sys::memory::{NR_MMAP, NR_MUNMAP};
use edge_libos::Kernel;

/// Linux x86-64 mmap flag bits, matching `mm/mod.rs`.
const MAP_ANONYMOUS: i64 = 0x20;
const MAP_PRIVATE: i64 = 0x02;

/// PROT_READ | PROT_WRITE.
const PROT_RW: i64 = 0x3;

/// WAT module that runs the full round-trip in wasm and returns the
/// byte-pattern it observed at offset 0 of the mmap'd region.
const ROUNDTRIP_WAT: &str = r#"
    (module
      (import "kernel" "syscall"
        (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
      (memory (export "memory") 1)

      ;; (mmap_roundtrip magic:i64) -> i64
      ;; 1. mmap 4096 anonymous RW pages.
      ;; 2. Store `magic` at offset 0 of the region.
      ;; 3. Read it back and return it. If the host zero-filled, this matches.
      ;; 4. Return -1 if mmap itself returned 0.
      (func (export "mmap_roundtrip") (param $magic i64) (result i64)
        (local $base i64)
        (local.set $base
          (call $syscall
            (i64.const 9)         ;; NR_MMAP
            (i64.const 0)         ;; addr hint
            (i64.const 4096)      ;; length
            (i64.const 3)         ;; PROT_READ | PROT_WRITE
            (i64.const 0x22)      ;; MAP_PRIVATE | MAP_ANONYMOUS
            (i64.const -1)        ;; fd
            (i64.const 0)))       ;; offset
        (if (i64.eqz (local.get $base))
          (then (return (i64.const -1))))
        ;; i32.store needs an i32 address; truncate high bits.
        (i32.store
          (i32.wrap_i64 (local.get $base))
          (i32.wrap_i64 (local.get $magic)))
        ;; Read it back and return as i64.
        (i64.extend_i32_u
          (i32.load
            (i32.wrap_i64 (local.get $base))))
      )
    )
"#;

/// WAT module that mmap/munmap/mmap in sequence. Returns the second
/// allocation's address (or -1 if either mmap failed).
const REUSE_AFTER_FREE_WAT: &str = r#"
    (module
      (import "kernel" "syscall"
        (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
      (memory (export "memory") 1)

      (func (export "alloc_free_alloc") (result i64)
        (local $a i64)
        (local $b i64)
        (local $status i64)
        (local.set $status (i64.const 0))
        ;; First mmap: 8192 bytes.
        (local.set $a
          (call $syscall
            (i64.const 9)         ;; NR_MMAP
            (i64.const 0)
            (i64.const 8192)
            (i64.const 3)         ;; PROT_READ | PROT_WRITE
            (i64.const 0x22)      ;; MAP_PRIVATE | MAP_ANONYMOUS
            (i64.const -1)
            (i64.const 0)))
        (if (i64.eqz (local.get $a))
          (then (local.set $status (i64.const -1))))
        ;; munmap(a, 8192)
        (drop
          (call $syscall
            (i64.const 11)        ;; NR_MUNMAP
            (local.get $a)
            (i64.const 8192)
            (i64.const 0)
            (i64.const 0)
            (i64.const 0)
            (i64.const 0)))
        ;; Second mmap: 4096 bytes.
        (local.set $b
          (call $syscall
            (i64.const 9)
            (i64.const 0)
            (i64.const 4096)
            (i64.const 3)
            (i64.const 0x22)
            (i64.const -1)
            (i64.const 0)))
        (if (i64.eqz (local.get $b))
          (then (local.set $status (i64.const -2))))
        ;; If status is 0, return b. Otherwise return status.
        (if (result i64) (i64.eqz (local.get $status))
          (then (local.get $b))
          (else (local.get $status)))
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

async fn run(
    engine: &wasmtime::Engine,
    linker: &wasmtime::Linker<Kernel>,
    wat: &str,
    fn_name: &str,
    args: &[wasmtime::Val],
) -> Result<i64> {
    let module = common::compile_wat(engine, wat)?;
    let (mut store, instance) = common::instantiate_async(engine, linker, &module).await?;
    let f = instance.get_typed_func::<(i64,), i64>(&mut store, fn_name)?;
    let ret = f.call_async(&mut store, (args[0].unwrap_i64(),)).await?;
    Ok(ret)
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
fn mmap_returns_nonzero_and_zero_fills() -> Result<()> {
    let (engine, linker) = common::engine_and_linker()?;
    let magic: i64 = 0x1234_5678;
    let observed = block_on(run(
        &engine,
        &linker,
        ROUNDTRIP_WAT,
        "mmap_roundtrip",
        &[wasmtime::Val::I64(magic)],
    ))?;
    assert_eq!(observed, magic, "mmap round-trip should preserve the written byte pattern (got {observed:#x}, want {magic:#x})");
    Ok(())
}

#[test]
fn munmap_then_mmap_works() -> Result<()> {
    let (engine, linker) = common::engine_and_linker()?;
    let second = block_on(run_noargs(
        &engine,
        &linker,
        REUSE_AFTER_FREE_WAT,
        "alloc_free_alloc",
    ))?;
    assert!(second > 0, "second mmap after munmap must return a positive address (got {second})");
    Ok(())
}

#[test]
fn mmap_rejects_zero_length() -> Result<()> {
    // Build a tiny wasm that mmaps len=0 and returns the host's reply.
    const WAT: &str = r#"
        (module
          (import "kernel" "syscall"
            (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
          (memory (export "memory") 1)
          (func (export "go") (result i64)
            (call $syscall
              (i64.const 9) (i64.const 0) (i64.const 0)
              (i64.const 3) (i64.const 0x22) (i64.const -1) (i64.const 0)))
        )
    "#;
    let (engine, linker) = common::engine_and_linker()?;
    let ret = block_on(run_noargs(&engine, &linker, WAT, "go"))?;
    assert_eq!(ret, -errno::EINVAL, "mmap with len=0 must return -EINVAL");
    Ok(())
}

#[test]
fn mmap_rejects_file_backed() -> Result<()> {
    // fd != -1 must return -ENOSYS (we don't have a host FD layer in P0).
    const WAT: &str = r#"
        (module
          (import "kernel" "syscall"
            (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
          (memory (export "memory") 1)
          (func (export "go") (result i64)
            (call $syscall
              (i64.const 9) (i64.const 0) (i64.const 4096)
              (i64.const 3) (i64.const 0x22) (i64.const 0) (i64.const 0)))
        )
    "#;
    let (engine, linker) = common::engine_and_linker()?;
    let ret = block_on(run_noargs(&engine, &linker, WAT, "go"))?;
    assert_eq!(ret, -errno::ENOSYS, "file-backed mmap must return -ENOSYS");
    Ok(())
}

#[test]
fn mmap_grows_linear_memory() -> Result<()> {
    // First 1-page (64 KiB) memory. FIRST_ARENA_BASE is 0x0010_0000 (1 MiB),
    // so placing an arena there requires growing memory to at least 1 MiB +
    // 256 KiB. After the guest calls mmap, store.data().memory().data_size
    // must reflect the grow.
    const WAT: &str = r#"
        (module
          (import "kernel" "syscall"
            (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
          (memory (export "memory") 1)
          (func (export "go") (result i64)
            (call $syscall
              (i64.const 9) (i64.const 0) (i64.const 4096)
              (i64.const 3) (i64.const 0x22) (i64.const -1) (i64.const 0)))
        )
    "#;
    let (engine, linker) = common::engine_and_linker()?;
    let module = common::compile_wat(&engine, WAT)?;
    let addr = block_on(async {
        let (mut store, instance) = common::instantiate_async(&engine, &linker, &module).await?;
        let f = instance.get_typed_func::<(), i64>(&mut store, "go")?;
        let ret = f.call_async(&mut store, ()).await?;
        Ok::<_, anyhow::Error>((ret, store))
    })?;
    assert!(addr.0 > 0, "mmap should succeed, got {}", addr.0);
    let store: Store<Kernel> = addr.1;
    let mem_size = store
        .data()
        .memory
        .as_ref()
        .expect("memory attached")
        .data_size(&store);
    let first_arena = edge_libos::mm::LinearAllocator::FIRST_ARENA_BASE as usize;
    let arena_size = edge_libos::mm::ARENA_SIZE;
    let required = first_arena + arena_size;
    assert!(
        mem_size >= required,
        "after mmap, memory should be >= {required} bytes (got {mem_size})"
    );
    Ok(())
}

#[test]
fn munmap_returns_einval_for_out_of_arena() -> Result<()> {
    // Munmap an address in [0, 64KiB) — inside the wasm's static region,
    // not in any arena we own.
    const WAT: &str = r#"
        (module
          (import "kernel" "syscall"
            (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
          (memory (export "memory") 1)
          (func (export "go") (result i64)
            (call $syscall
              (i64.const 11)     ;; NR_MUNMAP
              (i64.const 4096)   ;; addr: inside static region
              (i64.const 4096)   ;; len
              (i64.const 0)
              (i64.const 0)
              (i64.const 0)
              (i64.const 0)))
        )
    "#;
    let (engine, linker) = common::engine_and_linker()?;
    let ret = block_on(run_noargs(&engine, &linker, WAT, "go"))?;
    assert_eq!(ret, -errno::EINVAL, "munmap of unmapped range must return -EINVAL");
    Ok(())
}

#[test]
fn nr_constants_match_linux_x86_64() {
    assert_eq!(NR_MMAP, 9);
    assert_eq!(NR_MUNMAP, 11);
    assert_eq!(MAP_ANONYMOUS, 0x20);
    assert_eq!(MAP_PRIVATE, 0x02);
    assert_eq!(PROT_RW, 0x3);
}
