//! P1-3 WAT-level tests for `setsockopt(2)`, `fcntl(F_GETFL/F_SETFL)`,
//! and `pipe2(O_NONBLOCK)`.

mod common;

use anyhow::Result;

use edge_libos::Kernel;

fn block_on<F: std::future::Future>(f: F) -> F::Output {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio current_thread runtime");
    rt.block_on(f)
}

// WAT modules ---------------------------------------------------------------

/// `setsockopt(fd, level, optname, optval, optlen)` — no pointer dereference
/// in the kernel today; optval is bounds-checked but ignored. optlen=4 +
/// pointer at 4096 is the typical "int optval=1" shape.
const SETSOCKOPT_WAT: &str = r#"
    (module
      (import "kernel" "syscall"
        (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
      (memory (export "memory") 1)
      (data (i32.const 4096) "\01\00\00\00") ;; int optval = 1 (LE)
      (func (export "go")
        (param $fd i64) (param $level i64) (param $optname i64)
        (result i64)
        (call $syscall
          (i64.const 54)               ;; NR_SETSOCKOPT
          (local.get $fd)
          (local.get $level)
          (local.get $optname)
          (i64.const 4096)             ;; optval pointer
          (i64.const 4)                ;; optlen
          (i64.const 0)))
    )
"#;

/// `pipe2(fdarray_ptr, flags)` — writes [rd, wr] u32 little-endian to fdarray.
const PIPE2_WAT: &str = r#"
    (module
      (import "kernel" "syscall"
        (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
      (memory (export "memory") 1)
      (func (export "go") (param $flags i64) (result i64)
        (call $syscall
          (i64.const 293)              ;; NR_PIPE2
          (i64.const 4096)             ;; fdarray pointer
          (local.get $flags)
          (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0)))
    )
"#;

/// `fcntl(fd, cmd, arg)` — three-arg form.
const FCNTL_WAT: &str = r#"
    (module
      (import "kernel" "syscall"
        (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
      (memory (export "memory") 1)
      (func (export "go")
        (param $fd i64) (param $cmd i64) (param $arg i64)
        (result i64)
        (call $syscall
          (i64.const 72)               ;; NR_FCNTL
          (local.get $fd)
          (local.get $cmd)
          (local.get $arg)
          (i64.const 0) (i64.const 0) (i64.const 0)))
    )
"#;

/// `socket(family, type, proto)` — used to allocate a fresh socket fd.
const SOCKET_WAT: &str = r#"
    (module
      (import "kernel" "syscall"
        (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
      (memory (export "memory") 1)
      (func (export "go") (param $family i64) (param $ty i64) (result i64)
        (call $syscall
          (i64.const 41)                ;; NR_SOCKET
          (local.get $family)
          (local.get $ty)
          (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0)))
    )
"#;

// Helpers -------------------------------------------------------------------

async fn call_socket(
    linker: &wasmtime::Linker<Kernel>,
    store: &mut wasmtime::Store<Kernel>,
    module: &wasmtime::Module,
    family: i64,
    ty: i64,
) -> Result<i64> {
    let inst = linker.instantiate_async(&mut *store, module).await?;
    if let Some(mem) = inst.get_memory(&mut *store, "memory") {
        store.data_mut().attach_memory(mem);
    }
    let f = inst.get_typed_func::<(i64, i64), i64>(&mut *store, "go")?;
    Ok(f.call_async(&mut *store, (family, ty)).await?)
}

async fn call_setsockopt(
    linker: &wasmtime::Linker<Kernel>,
    store: &mut wasmtime::Store<Kernel>,
    module: &wasmtime::Module,
    fd: i64,
    level: i64,
    optname: i64,
) -> Result<i64> {
    let inst = linker.instantiate_async(&mut *store, module).await?;
    if let Some(mem) = inst.get_memory(&mut *store, "memory") {
        store.data_mut().attach_memory(mem);
    }
    let f = inst.get_typed_func::<(i64, i64, i64), i64>(&mut *store, "go")?;
    Ok(f.call_async(&mut *store, (fd, level, optname)).await?)
}

async fn call_pipe2(
    linker: &wasmtime::Linker<Kernel>,
    store: &mut wasmtime::Store<Kernel>,
    module: &wasmtime::Module,
    flags: i64,
) -> Result<i64> {
    let inst = linker.instantiate_async(&mut *store, module).await?;
    if let Some(mem) = inst.get_memory(&mut *store, "memory") {
        store.data_mut().attach_memory(mem);
    }
    let f = inst.get_typed_func::<i64, i64>(&mut *store, "go")?;
    Ok(f.call_async(&mut *store, flags).await?)
}

async fn call_fcntl(
    linker: &wasmtime::Linker<Kernel>,
    store: &mut wasmtime::Store<Kernel>,
    module: &wasmtime::Module,
    fd: i64,
    cmd: i64,
    arg: i64,
) -> Result<i64> {
    let inst = linker.instantiate_async(&mut *store, module).await?;
    if let Some(mem) = inst.get_memory(&mut *store, "memory") {
        store.data_mut().attach_memory(mem);
    }
    let f = inst.get_typed_func::<(i64, i64, i64), i64>(&mut *store, "go")?;
    Ok(f.call_async(&mut *store, (fd, cmd, arg)).await?)
}

/// Read two u32 little-endian fds from offset 4096 in linear memory.
fn read_fds(store: &mut wasmtime::Store<Kernel>) -> Result<(i64, i64)> {
    let mem = store
        .data()
        .memory
        .ok_or_else(|| anyhow::anyhow!("no guest memory attached"))?;
    let mut buf = [0u8; 8];
    mem.read(&mut *store, 4096, &mut buf)
        .map_err(|e| anyhow::anyhow!("read failed: {e}"))?;
    let rd = i32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as i64;
    let wr = i32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]) as i64;
    Ok((rd, wr))
}

// Tests ---------------------------------------------------------------------

/// `setsockopt(AF_INET_STREAM, SOL_SOCKET, SO_REUSEADDR=2)` returns 0.
#[test]
fn setsockopt_so_reuseaddr_returns_zero() -> Result<()> {
    let (engine, linker) = common::engine_and_linker()?;
    let sock = common::compile_wat(&engine, SOCKET_WAT)?;
    let so = common::compile_wat(&engine, SETSOCKOPT_WAT)?;

    let ret = block_on(async {
        let mut store = edge_libos::build_store(&engine, Kernel::new(vec![], vec![]));
        let fd = call_socket(&linker, &mut store, &sock, 2, 1).await?;
        call_setsockopt(
            &linker, &mut store, &so, fd, 1, /*SOL_SOCKET*/
            2, /*SO_REUSEADDR*/
        )
        .await
    })?;
    assert_eq!(ret, 0);
    Ok(())
}

/// `setsockopt(AF_INET_STREAM, IPPROTO_TCP, TCP_NODELAY=1)` returns 0.
#[test]
fn setsockopt_tcp_nodelay_returns_zero() -> Result<()> {
    let (engine, linker) = common::engine_and_linker()?;
    let sock = common::compile_wat(&engine, SOCKET_WAT)?;
    let so = common::compile_wat(&engine, SETSOCKOPT_WAT)?;

    let ret = block_on(async {
        let mut store = edge_libos::build_store(&engine, Kernel::new(vec![], vec![]));
        let fd = call_socket(&linker, &mut store, &sock, 2, 1).await?;
        call_setsockopt(
            &linker, &mut store, &so, fd, 6, /*IPPROTO_TCP*/
            1, /*TCP_NODELAY*/
        )
        .await
    })?;
    assert_eq!(ret, 0);
    Ok(())
}

/// `setsockopt(fd, 999, 999, ...)` (unknown level + optname) returns 0.
#[test]
fn setsockopt_unknown_returns_zero() -> Result<()> {
    let (engine, linker) = common::engine_and_linker()?;
    let sock = common::compile_wat(&engine, SOCKET_WAT)?;
    let so = common::compile_wat(&engine, SETSOCKOPT_WAT)?;

    let ret = block_on(async {
        let mut store = edge_libos::build_store(&engine, Kernel::new(vec![], vec![]));
        let fd = call_socket(&linker, &mut store, &sock, 2, 1).await?;
        call_setsockopt(&linker, &mut store, &so, fd, 999, 999).await
    })?;
    assert_eq!(ret, 0);
    Ok(())
}

/// `setsockopt(fd=stdin=0, ...)` returns -EBADF (not a Socket).
#[test]
fn setsockopt_on_non_socket_returns_ebadf() -> Result<()> {
    let (engine, linker) = common::engine_and_linker()?;
    let so = common::compile_wat(&engine, SETSOCKOPT_WAT)?;

    let ret = block_on(async {
        let mut store = edge_libos::build_store(&engine, Kernel::new(vec![], vec![]));
        call_setsockopt(&linker, &mut store, &so, 0 /*stdin*/, 1, 2).await
    })?;
    assert_eq!(ret, -edge_libos::errno::EBADF);
    Ok(())
}

/// `pipe2(O_NONBLOCK=0o4000)` creates a nonblocking pair. We verify by
/// reading F_GETFL on the read end and checking `O_NONBLOCK` is set.
#[test]
fn pipe2_o_nonblock_sets_flag_on_pair() -> Result<()> {
    let (engine, linker) = common::engine_and_linker()?;
    let pipe = common::compile_wat(&engine, PIPE2_WAT)?;
    let fcntl_mod = common::compile_wat(&engine, FCNTL_WAT)?;

    let ret = block_on(async {
        let mut store = edge_libos::build_store(&engine, Kernel::new(vec![], vec![]));
        let rc = call_pipe2(&linker, &mut store, &pipe, 0o4000).await?;
        assert_eq!(rc, 0, "pipe2 should return 0");
        let (rd, _wr) = read_fds(&mut store)?;
        // F_GETFL(rd) → O_RDONLY | O_NONBLOCK = 0 | 0o4000
        call_fcntl(&linker, &mut store, &fcntl_mod, rd, 3 /*F_GETFL*/, 0).await
    })?;
    assert_eq!(
        ret, 0o4000,
        "F_GETFL on nonblocking pipe read end should report O_NONBLOCK"
    );
    Ok(())
}

/// `fcntl(F_SETFL, 0)` clears O_NONBLOCK on the read end of a previously
/// nonblocking pipe.
#[test]
fn fcntl_setfl_zero_clears_o_nonblock() -> Result<()> {
    let (engine, linker) = common::engine_and_linker()?;
    let pipe = common::compile_wat(&engine, PIPE2_WAT)?;
    let fcntl_mod = common::compile_wat(&engine, FCNTL_WAT)?;

    let (after_clear, after_set) = block_on(async {
        let mut store = edge_libos::build_store(&engine, Kernel::new(vec![], vec![]));
        let rc = call_pipe2(&linker, &mut store, &pipe, 0).await?; // blocking pair
        assert_eq!(rc, 0);
        let (rd, _wr) = read_fds(&mut store)?;
        // Sanity: starts blocking (no O_NONBLOCK).
        let initial = call_fcntl(&linker, &mut store, &fcntl_mod, rd, 3 /*F_GETFL*/, 0).await?;
        assert_eq!(
            initial & 0o4000,
            0,
            "blocking pipe should not report O_NONBLOCK initially"
        );
        // Flip to nonblocking.
        let set_rc = call_fcntl(
            &linker, &mut store, &fcntl_mod, rd, 4, /*F_SETFL*/
            0o4000,
        )
        .await?;
        assert_eq!(set_rc, 0, "F_SETFL=O_NONBLOCK should return 0");
        // Now F_GETFL should report O_NONBLOCK.
        let after_set = call_fcntl(&linker, &mut store, &fcntl_mod, rd, 3 /*F_GETFL*/, 0).await?;
        assert_eq!(
            after_set & 0o4000,
            0o4000,
            "O_NONBLOCK should be set after F_SETFL=0o4000"
        );
        // Clear it.
        let clear_rc = call_fcntl(&linker, &mut store, &fcntl_mod, rd, 4 /*F_SETFL*/, 0).await?;
        assert_eq!(clear_rc, 0, "F_SETFL=0 should return 0");
        let after_clear = call_fcntl(&linker, &mut store, &fcntl_mod, rd, 3 /*F_GETFL*/, 0).await?;
        assert_eq!(
            after_clear & 0o4000,
            0,
            "O_NONBLOCK should be cleared after F_SETFL=0"
        );
        Ok::<_, anyhow::Error>((after_clear, after_set))
    })?;
    let _ = (after_clear, after_set);
    Ok(())
}
