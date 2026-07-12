//! Smoke test: load a tiny WAT module, run it, confirm our dispatch + linker
//! wiring works end-to-end.
//!
//! Step 4 of the P0 build order. The fixture module imports `kernel.syscall`
//! and immediately calls it with NR_WRITE + a pointer to a string constant.
//! As of Step 12 NR_WRITE is wired into the dispatch and the buffered-stdio
//! pipe, so `_start` returns the byte count (4) instead of -ENOSYS. The
//! companion test `dispatch_handles_a_nonexistent_syscall_number` still
//! verifies that truly unknown numbers return -ENOSYS.

mod common;

use anyhow::Result;

use edge_libos::sys::file::NR_WRITE;
use edge_libos::Kernel;

/// Run an async block on a fresh single-threaded tokio runtime.
///
/// wasmtime 45.0.3's async host functions require `Config::async_support(true)`,
/// which means every `call`, `func_wrap`, and instance-method access that
/// crosses the host/guest boundary must be `*_async`. We drive those via
/// `tokio::runtime::Runtime::block_on`.
fn block_on<F: std::future::Future>(f: F) -> F::Output {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio current_thread runtime");
    rt.block_on(f)
}

/// A WAT module that:
/// 1. Stores a 4-byte string "hi\n" in linear memory at offset 0x100.
/// 2. Calls `kernel.syscall(NR_WRITE, 1, 0x100, 4)` — host writes 4 bytes
///    into the buffered stdout pipe and returns the byte count.
/// 3. Returns the byte count as the function's return value.
const SMOKE_WAT: &str = r#"
    (module
      (import "kernel" "syscall"
        (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
      (memory (export "memory") 1)
      (data (i32.const 0x100) "hi\n")
      (func (export "_start") (result i64)
        ;; NR_WRITE = 1, fd=1, ptr=0x100, len=4
        (call $syscall (i64.const 1) (i64.const 1) (i64.const 0x100) (i64.const 4) (i64.const 0) (i64.const 0) (i64.const 0))
      )
    )
"#;

#[test]
fn dispatch_routes_write_to_implementation() -> Result<()> {
    // NR_WRITE is now wired; the host must drain 4 bytes ("hi\n") into the
    // buffered stdout pipe and return the byte count.
    let (engine, linker) = common::engine_and_linker()?;
    let module = common::compile_wat(&engine, SMOKE_WAT)?;

    block_on(async {
        let (mut store, instance) = common::instantiate_async(&engine, &linker, &module).await?;
        let start = instance.get_typed_func::<(), i64>(&mut store, "_start")?;
        let ret = start.call_async(&mut store, ()).await?;
        assert_eq!(
            ret, 4,
            "expected 4 bytes written from dispatched NR_WRITE, got {ret}"
        );
        assert!(store.data().memory.as_ref().is_some());
        Ok::<(), anyhow::Error>(())
    })
}

#[test]
fn dispatch_handles_a_nonexistent_syscall_number() -> Result<()> {
    // Caller asks for syscall 9999 which we don't implement. The default
    // arm of the dispatch must return -ENOSYS (not crash, not a raw errno).
    const WAT: &str = r#"
        (module
          (import "kernel" "syscall"
            (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
          (memory (export "memory") 1)
          (func (export "call_unknown") (result i64)
            (call $syscall (i64.const 9999) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0))
          )
        )
    "#;
    let (engine, linker) = common::engine_and_linker()?;
    let module = common::compile_wat(&engine, WAT)?;

    block_on(async {
        let (mut store, instance) = common::instantiate_async(&engine, &linker, &module).await?;
        let f = instance.get_typed_func::<(), i64>(&mut store, "call_unknown")?;
        let ret = f.call_async(&mut store, ()).await?;
        assert_eq!(ret, -edge_libos::errno::ENOSYS);
        Ok::<(), anyhow::Error>(())
    })
}

#[test]
fn nr_write_constant_matches_linux_x86_64() {
    // Sanity check that the syscall number constants match Linux x86-64,
    // since the guest libc was built against those numbers.
    assert_eq!(NR_WRITE, 1, "NR_WRITE must be 1 (Linux x86-64 unistd_64.h)");
    assert_eq!(edge_libos::sys::process::NR_EXIT, 60);
    assert_eq!(edge_libos::sys::process::NR_EXIT_GROUP, 231);
    assert_eq!(edge_libos::sys::memory::NR_MMAP, 9);
    assert_eq!(edge_libos::sys::memory::NR_BRK, 12);
    assert_eq!(edge_libos::sys::file::NR_GETDENTS64, 217);
    assert_eq!(edge_libos::sys::file::NR_OPENAT, 257);
    assert_eq!(edge_libos::sys::random::NR_GETRANDOM, 318);
    assert_eq!(edge_libos::sys::time::NR_CLOCK_GETTIME, 228);
}

#[test]
fn kernel_new_constructs_with_default_state() {
    use edge_libos::fd::{STDERR, STDIN, STDOUT};

    let k = Kernel::new(
        vec!["python".into(), "hello.py".into()],
        vec![("PYTHONUNBUFFERED".into(), "1".into())],
    );
    assert_eq!(k.args.len(), 2);
    assert_eq!(k.env.len(), 1);
    assert!(k.memory.is_none(), "memory should not be attached at construction");
    assert!(k.fds.contains(STDIN), "fd 0 (stdin) must be preloaded");
    assert!(k.fds.contains(STDOUT), "fd 1 (stdout) must be preloaded");
    assert!(k.fds.contains(STDERR), "fd 2 (stderr) must be preloaded");
}
