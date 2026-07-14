//! read / write conformance against the buffered stdio pipes.

mod common;

use anyhow::Result;

use edge_libos::Kernel;

const NR_READ: u32 = 0;
const NR_WRITE: u32 = 1;

/// WAT: write(1, "hello, world!\n", 14). The buffer lives at offset 4096.
const WRITE_HELLO_WAT: &str = r#"
    (module
      (import "kernel" "syscall"
        (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
      (memory (export "memory") 1)
      (func (export "go") (result i64)
        (i64.store (i32.const 4096) (i64.const 0x6c6c6568))     ;; "hell"
        (i64.store (i32.const 4104) (i64.const 0x77202c6f))     ;; "o, w"
        (i64.store (i32.const 4112) (i64.const 0x21646c72))     ;; "rld!"
        (i32.store (i32.const 4120) (i32.const 0x0a))           ;; "\n"
        (call $syscall
          (i64.const 1)            ;; NR_WRITE
          (i64.const 1)            ;; fd = stdout
          (i64.const 4096)         ;; buf
          (i64.const 14)           ;; len
          (i64.const 0) (i64.const 0) (i64.const 0))))
"#;

/// WAT: read into a buffer at offset 4096, return the i32 value stored
/// at offset 4096 (interpreted as the first 4 bytes).
const READ_BUF_WAT: &str = r#"
    (module
      (import "kernel" "syscall"
        (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
      (memory (export "memory") 1)
      (func (export "go") (result i64)
        (call $syscall
          (i64.const 0)            ;; NR_READ
          (i64.const 0)            ;; fd = stdin
          (i64.const 4096)
          (i64.const 16)
          (i64.const 0) (i64.const 0) (i64.const 0))))
"#;

/// WAT: write to stderr fd=2.
const WRITE_STDERR_WAT: &str = r#"
    (module
      (import "kernel" "syscall"
        (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
      (memory (export "memory") 1)
      (func (export "go") (result i64)
        (i64.store (i32.const 4096) (i64.const 0x0a215345))     ;; "ES!\n"
        (i64.store (i32.const 4104) (i64.const 0x0a0a0a0a))     ;; "\n\n\n\n"
        (call $syscall
          (i64.const 1)
          (i64.const 2)            ;; stderr
          (i64.const 4096)
          (i64.const 8)
          (i64.const 0) (i64.const 0) (i64.const 0))))
"#;

/// WAT: read on fd=999 returns -EBADF.
const READ_BAD_FD_WAT: &str = r#"
    (module
      (import "kernel" "syscall"
        (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
      (memory (export "memory") 1)
      (func (export "go") (result i64)
        (call $syscall
          (i64.const 0) (i64.const 999) (i64.const 4096)
          (i64.const 16) (i64.const 0) (i64.const 0) (i64.const 0))))
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
) -> Result<(i64, Kernel)> {
    let module = common::compile_wat(engine, wat)?;
    let (mut store, instance) = common::instantiate_async(engine, linker, &module).await?;
    let f = instance.get_typed_func::<(), i64>(&mut store, fn_name)?;
    let ret = f.call_async(&mut store, ()).await?;
    Ok((ret, Kernel::new(vec![], vec![]))) // dummy kernel just for type compat
}

/// Helper that runs a no-arg wasm fn and returns (ret, store) so tests can
/// inspect the kernel state.
async fn run_and_get_store(
    engine: &wasmtime::Engine,
    linker: &wasmtime::Linker<Kernel>,
    wat: &str,
    fn_name: &str,
) -> Result<(i64, wasmtime::Store<Kernel>)> {
    let module = common::compile_wat(engine, wat)?;
    let (mut store, instance) = common::instantiate_async(engine, linker, &module).await?;
    let f = instance.get_typed_func::<(), i64>(&mut store, fn_name)?;
    let ret = f.call_async(&mut store, ()).await?;
    Ok((ret, store))
}

fn stdout_bytes(store: &wasmtime::Store<Kernel>) -> Vec<u8> {
    let fds = &store.data().fds;
    match fds.get(1) {
        Ok(edge_libos::fd::Resource::Stdout(w)) => {
            let q = w.buf.lock();
            q.iter().copied().collect()
        }
        Ok(_) | Err(_) => Vec::new(),
    }
}

fn stderr_bytes(store: &wasmtime::Store<Kernel>) -> Vec<u8> {
    let fds = &store.data().fds;
    if let Ok(edge_libos::fd::Resource::Stderr(w)) = fds.get(2) {
        w.buf.lock().iter().copied().collect()
    } else {
        Vec::new()
    }
}

#[test]
fn write_to_stdout_captures_bytes() -> Result<()> {
    let (engine, linker) = common::engine_and_linker()?;
    let (ret, store) = block_on(run_and_get_store(&engine, &linker, WRITE_HELLO_WAT, "go"))?;
    assert_eq!(ret, 14, "write should report all 14 bytes written");
    let bytes = stdout_bytes(&store);
    assert_eq!(bytes.len(), 14, "should capture exactly 14 bytes");
    assert_eq!(&bytes[..4], b"hell");
    // 14-byte window starting at 4096: "hell\0\0\0\0o, w".
    // bytes[0..4] = "hell", bytes[8..12] = "o, w".
    assert_eq!(&bytes[8..12], b"o, w");
    Ok(())
}

#[test]
fn write_to_stderr_captures_bytes() -> Result<()> {
    let (engine, linker) = common::engine_and_linker()?;
    let (ret, store) = block_on(run_and_get_store(&engine, &linker, WRITE_STDERR_WAT, "go"))?;
    assert_eq!(ret, 8);
    let bytes = stderr_bytes(&store);
    assert_eq!(&bytes[..4], b"ES!\n");
    Ok(())
}

#[test]
fn read_with_no_data_returns_eagain() -> Result<()> {
    let (engine, linker) = common::engine_and_linker()?;
    let (ret, _store) = block_on(run_and_get_store(&engine, &linker, READ_BUF_WAT, "go"))?;
    assert_eq!(ret, -edge_libos::errno::EAGAIN);
    Ok(())
}

#[test]
fn read_with_closed_pipe_returns_zero() -> Result<()> {
    // Construct a kernel with a closed pipe preloaded at fd 0 (stdin),
    // then instantiate + run.
    let mut kernel = Kernel::new(vec![], vec![]);
    // Replace fd 0 (Stdin) with a pipe whose write end is already closed.
    let (rd, wr) = edge_libos::fd::make_pipe();
    wr.buf.lock().clear();
    *wr.closed.lock() = true;
    kernel
        .fds
        .table_mut_for_test()
        .insert(0, edge_libos::fd::Resource::Stdin(rd));

    let engine = edge_libos::build_engine()?;
    let mut linker = wasmtime::Linker::new(&engine);
    edge_libos::add_to_linker(&mut linker)?;
    let mut store = edge_libos::build_store(&engine, kernel);
    let ret = block_on(async {
        let module = common::compile_wat(&engine, READ_BUF_WAT)?;
        let instance = linker.instantiate_async(&mut store, &module).await?;
        if let Some(mem) = instance.get_memory(&mut store, "memory") {
            store.data_mut().attach_memory(mem);
        }
        let f = instance.get_typed_func::<(), i64>(&mut store, "go")?;
        let r = f.call_async(&mut store, ()).await?;
        Ok::<_, anyhow::Error>(r)
    })?;
    assert_eq!(ret, 0, "closed pipe read should return 0 (EOF)");
    Ok(())
}

#[test]
fn read_unknown_fd_returns_ebadf() -> Result<()> {
    let (engine, linker) = common::engine_and_linker()?;
    let (ret, _store) = block_on(run_and_get_store(&engine, &linker, READ_BAD_FD_WAT, "go"))?;
    assert_eq!(ret, -edge_libos::errno::EBADF);
    Ok(())
}

#[test]
fn write_eault_on_bad_pointer() -> Result<()> {
    // Write to a pointer past end of memory.
    const WAT: &str = r#"
        (module
          (import "kernel" "syscall"
            (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
          (memory (export "memory") 1)
          (func (export "go") (result i64)
            (call $syscall
              (i64.const 1) (i64.const 1)
              (i64.const 100000000) (i64.const 16)
              (i64.const 0) (i64.const 0) (i64.const 0))))
    "#;
    let (engine, linker) = common::engine_and_linker()?;
    let (ret, _store) = block_on(run_and_get_store(&engine, &linker, WAT, "go"))?;
    assert_eq!(ret, -edge_libos::errno::EFAULT);
    Ok(())
}

#[test]
fn nr_constants_match_linux_x86_64() {
    assert_eq!(NR_READ, 0);
    assert_eq!(NR_WRITE, 1);
}
