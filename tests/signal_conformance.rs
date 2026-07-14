//! rt_sigaction / rt_sigprocmask conformance.

mod common;

use anyhow::Result;

use edge_libos::Kernel;

/// Build a sigaction struct in guest memory at offset 4096 with the given
/// handler, flags, and mask. Returns a wasm module whose `go` function
/// invokes `rt_sigaction(signum, @4096, @4096+32, 8)` and returns 0 if
/// the host accepted it.
const SIGACTION_WAT: &str = r#"
    (module
      (import "kernel" "syscall"
        (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
      (memory (export "memory") 1)
      (func (export "install") (param $signum i64) (param $handler i64)
                              (param $flags i64) (param $mask i64)
                              (result i64)
        ;; Write sigaction struct at offset 4096 (32 bytes):
        ;;   sa_handler: u32 @ 4096
        ;;   sa_flags: u32 @ 4100
        ;;   sa_mask: u64 @ 4104 (uses first 8 bytes; musl padding extends to 16)
        ;;   sa_restorer: u32 @ 4124 (writes as 0)
        (i32.store (i32.const 4096)
          (i32.wrap_i64 (local.get $handler)))
        (i32.store (i32.const 4100)
          (i32.wrap_i64 (local.get $flags)))
        (i64.store (i32.const 4104) (local.get $mask))
        (i32.store (i32.const 4124) (i32.const 0))
        (call $syscall
          (i64.const 13)            ;; NR_RT_SIGACTION
          (local.get $signum)
          (i64.const 4096)
          (i64.const 0)             ;; oldact = NULL (don't query)
          (i64.const 8)
          (i64.const 0) (i64.const 0))))
"#;

/// Read sigaction struct back. `install` was followed by a query call that
/// writes oldact into the same @4096 buffer; this returns the recorded
/// sa_handler as i64.
const SIGACTION_QUERY_WAT: &str = r#"
    (module
      (import "kernel" "syscall"
        (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
      (memory (export "memory") 1)
      (func (export "install_then_query") (param $signum i64) (param $handler i64)
                                          (param $flags i64) (param $mask i64)
                                          (result i64)
        ;; Install
        (i32.store (i32.const 4096)
          (i32.wrap_i64 (local.get $handler)))
        (i32.store (i32.const 4100)
          (i32.wrap_i64 (local.get $flags)))
        (i64.store (i32.const 4104) (local.get $mask))
        (i32.store (i32.const 4124) (i32.const 0))
        (drop
          (call $syscall
            (i64.const 13)
            (local.get $signum)
            (i64.const 4096)
            (i64.const 0)
            (i64.const 8)
            (i64.const 0) (i64.const 0)))
        ;; Query (writes oldact into @4096+32 = 4128)
        (drop
          (call $syscall
            (i64.const 13)
            (local.get $signum)
            (i64.const 0)         ;; act = NULL
            (i64.const 4128)
            (i64.const 8)
            (i64.const 0) (i64.const 0)))
        ;; Return the handler that was just recorded.
        (i64.extend_i32_u (i32.load (i32.const 4128)))))
"#;

/// rt_sigprocmask(SIG_SETMASK, set@4096, oldset@4096+8, 8).
const SIGPROCMASK_WAT: &str = r#"
    (module
      (import "kernel" "syscall"
        (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
      (memory (export "memory") 1)
      (func (export "set_mask") (param $new_mask i64) (result i64)
        (i64.store (i32.const 4096) (local.get $new_mask))
        (call $syscall
          (i64.const 14)            ;; NR_RT_SIGPROCMASK
          (i64.const 2)             ;; SIG_SETMASK
          (i64.const 4096)
          (i64.const 0)             ;; oldset = NULL
          (i64.const 8)
          (i64.const 0) (i64.const 0))))
"#;

fn block_on<F: std::future::Future>(f: F) -> F::Output {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio current_thread runtime");
    rt.block_on(f)
}

#[allow(clippy::too_many_arguments)]
async fn run_sigaction(
    engine: &wasmtime::Engine,
    linker: &wasmtime::Linker<Kernel>,
    wat: &str,
    fn_name: &str,
    signum: i32,
    handler: u64,
    flags: u64,
    mask: u64,
) -> Result<(i64, Option<edge_libos::sys::signal::SigAction>)> {
    let module = common::compile_wat(engine, wat)?;
    let (mut store, instance) = common::instantiate_async(engine, linker, &module).await?;
    let f = instance.get_typed_func::<(i64, i64, i64, i64), i64>(&mut store, fn_name)?;
    let ret = f
        .call_async(
            &mut store,
            (signum as i64, handler as i64, flags as i64, mask as i64),
        )
        .await?;
    let recorded = store.data().signals.actions.get(&signum).copied();
    Ok((ret, recorded))
}

#[test]
fn sigaction_install_records_handler() -> Result<()> {
    let (engine, linker) = common::engine_and_linker()?;
    let (ret, recorded) = block_on(run_sigaction(
        &engine,
        &linker,
        SIGACTION_WAT,
        "install",
        13,
        0xdead_beef,
        0x42,
        0x1234,
    ))?;
    assert_eq!(ret, 0, "rt_sigaction should return 0");
    let sa = recorded.expect("handler should be recorded for signum 13");
    assert_eq!(sa.handler, 0xdead_beef);
    assert_eq!(sa.flags, 0x42);
    assert_eq!(sa.mask, 0x1234);
    Ok(())
}

#[test]
fn sigaction_query_returns_installed() -> Result<()> {
    let (engine, linker) = common::engine_and_linker()?;
    let module = common::compile_wat(&engine, SIGACTION_QUERY_WAT)?;
    let handler = block_on(async {
        let (mut store, instance) = common::instantiate_async(&engine, &linker, &module).await?;
        let f = instance
            .get_typed_func::<(i64, i64, i64, i64), i64>(&mut store, "install_then_query")?;
        let h = f
            .call_async(&mut store, (9_i64, 0xCAFE_BABE, 0, 0xFFFF))
            .await?;
        Ok::<_, anyhow::Error>(h)
    })?;
    assert_eq!(
        handler, 0xCAFE_BABE,
        "oldact should reflect the installed handler"
    );
    Ok(())
}

#[test]
fn sigaction_rejects_invalid_signum() -> Result<()> {
    const WAT: &str = r#"
        (module
          (import "kernel" "syscall"
            (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
          (memory (export "memory") 1)
          (func (export "go") (result i64)
            (call $syscall
              (i64.const 13)
              (i64.const 999)            ;; invalid signum
              (i64.const 0) (i64.const 0) (i64.const 8)
              (i64.const 0) (i64.const 0))))
    "#;
    let (engine, linker) = common::engine_and_linker()?;
    let module = common::compile_wat(&engine, WAT)?;
    let ret = block_on(async {
        let (mut store, instance) = common::instantiate_async(&engine, &linker, &module).await?;
        let f = instance.get_typed_func::<(), i64>(&mut store, "go")?;
        let r = f.call_async(&mut store, ()).await?;
        Ok::<_, anyhow::Error>(r)
    })?;
    assert_eq!(ret, -edge_libos::errno::EINVAL);
    Ok(())
}

#[test]
fn sigprocmask_set_then_query() -> Result<()> {
    let (engine, linker) = common::engine_and_linker()?;
    let module = common::compile_wat(&engine, SIGPROCMASK_WAT)?;
    let observed = block_on(async {
        let (mut store, instance) = common::instantiate_async(&engine, &linker, &module).await?;
        let f = instance.get_typed_func::<(i64,), i64>(&mut store, "set_mask")?;
        let r = f.call_async(&mut store, (0xABCD_i64,)).await?;
        assert_eq!(r, 0);
        Ok::<_, anyhow::Error>(store.data().signals.mask)
    })?;
    assert_eq!(observed, 0xABCD, "kernel must record the new mask");
    Ok(())
}

#[test]
fn sigprocmask_rejects_invalid_how() -> Result<()> {
    const WAT: &str = r#"
        (module
          (import "kernel" "syscall"
            (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
          (memory (export "memory") 1)
          (func (export "go") (result i64)
            (i64.store (i32.const 4096) (i64.const 1))
            (call $syscall
              (i64.const 14) (i64.const 99)
              (i64.const 4096) (i64.const 0)
              (i64.const 8) (i64.const 0) (i64.const 0))))
    "#;
    let (engine, linker) = common::engine_and_linker()?;
    let module = common::compile_wat(&engine, WAT)?;
    let ret = block_on(async {
        let (mut store, instance) = common::instantiate_async(&engine, &linker, &module).await?;
        let f = instance.get_typed_func::<(), i64>(&mut store, "go")?;
        let r = f.call_async(&mut store, ()).await?;
        Ok::<_, anyhow::Error>(r)
    })?;
    assert_eq!(ret, -edge_libos::errno::EINVAL);
    Ok(())
}

#[test]
fn nr_constants_match_linux_x86_64() {
    assert_eq!(edge_libos::sys::signal::NR_RT_SIGACTION, 13);
    assert_eq!(edge_libos::sys::signal::NR_RT_SIGPROCMASK, 14);
}
