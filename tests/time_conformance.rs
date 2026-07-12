//! Time syscalls: clock_gettime / gettimeofday / nanosleep conformance.

mod common;

use anyhow::Result;

use edge_libos::Kernel;

const CLOCK_REALTIME: i64 = 0;
const CLOCK_MONOTONIC: i64 = 1;

/// WAT: clock_gettime(CLOCK_REALTIME, ts@4096). Returns 0 if host wrote a
/// sane timespec; negative otherwise.
const CLOCK_GETTIME_REALTIME_WAT: &str = r#"
    (module
      (import "kernel" "syscall"
        (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
      (memory (export "memory") 1)
      (func (export "check_realtime") (result i64)
        (local $ret i64)
        (local $sec i64)
        (local $nsec i64)
        (local.set $ret
          (call $syscall
            (i64.const 228)
            (i64.const 0)
            (i64.const 4096)
            (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0)))
        (if (result i64) (i64.eqz (local.get $ret))
          (then
            (local.set $sec (i64.load (i32.const 4096)))
            (local.set $nsec (i64.load (i32.const 4104)))
            (if (i64.eqz (local.get $sec))
              (then (return (i64.const -1))))
            (if (i64.lt_s (local.get $nsec) (i64.const 0))
              (then (return (i64.const -2))))
            (if (i64.ge_s (local.get $nsec) (i64.const 1000000000))
              (then (return (i64.const -3))))
            (return (i64.const 0)))
          (else (return (i64.const -100))))))
"#;

/// WAT: clock_gettime(CLOCK_MONOTONIC, ts@4096). Returns the sec field.
const CLOCK_GETTIME_MONOTONIC_WAT: &str = r#"
    (module
      (import "kernel" "syscall"
        (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
      (memory (export "memory") 1)
      (func (export "check_monotonic") (result i64)
        (local $ret i64)
        (local.set $ret
          (call $syscall
            (i64.const 228)
            (i64.const 1)
            (i64.const 4096)
            (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0)))
        (if (result i64) (i64.eqz (local.get $ret))
          (then (return (i64.load (i32.const 4096))))
          (else (return (i64.const -1))))))
"#;

/// WAT: unknown clockid must return -EINVAL.
const BAD_CLOCK_WAT: &str = r#"
    (module
      (import "kernel" "syscall"
        (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
      (memory (export "memory") 1)
      (func (export "go") (result i64)
        (call $syscall
          (i64.const 228) (i64.const 99999) (i64.const 4096)
          (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0))))
"#;

/// WAT: bad pointer must return -EFAULT.
const BAD_PTR_WAT: &str = r#"
    (module
      (import "kernel" "syscall"
        (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
      (memory (export "memory") 1)
      (func (export "go") (result i64)
        (call $syscall
          (i64.const 228) (i64.const 0) (i64.const 100000000)
          (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0))))
"#;

/// WAT: gettimeofday(timeval@4096, NULL). Validates usec field.
const GETTIMEOFDAY_WAT: &str = r#"
    (module
      (import "kernel" "syscall"
        (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
      (memory (export "memory") 1)
      (func (export "go") (result i64)
        (local $ret i64)
        (local $usec i64)
        (local.set $ret
          (call $syscall
            (i64.const 96)
            (i64.const 4096)
            (i64.const 0)
            (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0)))
        (if (result i64) (i64.eqz (local.get $ret))
          (then
            (local.set $usec (i64.load (i32.const 4104)))
            (if (i64.lt_s (local.get $usec) (i64.const 0))
              (then (return (i64.const -1))))
            (if (i64.ge_s (local.get $usec) (i64.const 1000000))
              (then (return (i64.const -2))))
            (return (i64.const 0)))
          (else (return (i64.const -100))))))
"#;

/// WAT: nanosleep for 10ms. Returns the host's reply.
const NANOSLEEP_WAT: &str = r#"
    (module
      (import "kernel" "syscall"
        (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
      (memory (export "memory") 1)
      (func (export "go") (result i64)
        (i64.store (i32.const 4096) (i64.const 0))
        (i64.store (i32.const 4104) (i64.const 10000000))
        (call $syscall
          (i64.const 35)
          (i64.const 4096)
          (i64.const 0)
          (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0))))
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
fn clock_gettime_realtime_writes_sane_timespec() -> Result<()> {
    let (engine, linker) = common::engine_and_linker()?;
    let ret = block_on(run_noargs(
        &engine,
        &linker,
        CLOCK_GETTIME_REALTIME_WAT,
        "check_realtime",
    ))?;
    assert_eq!(
        ret, 0,
        "clock_gettime(CLOCK_REALTIME) must produce a sane timespec, got {ret}"
    );
    Ok(())
}

#[test]
fn clock_gettime_monotonic_returns_seconds() -> Result<()> {
    let (engine, linker) = common::engine_and_linker()?;
    let sec = block_on(run_noargs(
        &engine,
        &linker,
        CLOCK_GETTIME_MONOTONIC_WAT,
        "check_monotonic",
    ))?;
    assert!(sec >= 0, "clock_gettime(CLOCK_MONOTONIC) failed, got {sec}");
    Ok(())
}

#[test]
fn clock_gettime_rejects_unknown_clockid() -> Result<()> {
    let (engine, linker) = common::engine_and_linker()?;
    let ret = block_on(run_noargs(&engine, &linker, BAD_CLOCK_WAT, "go"))?;
    assert_eq!(ret, -edge_libos::errno::EINVAL);
    Ok(())
}

#[test]
fn clock_gettime_eault_on_bad_pointer() -> Result<()> {
    let (engine, linker) = common::engine_and_linker()?;
    let ret = block_on(run_noargs(&engine, &linker, BAD_PTR_WAT, "go"))?;
    assert_eq!(ret, -edge_libos::errno::EFAULT);
    Ok(())
}

#[test]
fn gettimeofday_writes_sane_timeval() -> Result<()> {
    let (engine, linker) = common::engine_and_linker()?;
    let ret = block_on(run_noargs(&engine, &linker, GETTIMEOFDAY_WAT, "go"))?;
    assert_eq!(ret, 0, "gettimeofday must produce sane timeval, got {ret}");
    Ok(())
}

#[test]
fn nanosleep_returns_zero_after_sleep() -> Result<()> {
    let (engine, linker) = common::engine_and_linker()?;
    let ret = block_on(run_noargs(&engine, &linker, NANOSLEEP_WAT, "go"))?;
    assert_eq!(ret, 0, "nanosleep(10ms) must return 0");
    Ok(())
}

#[test]
fn nr_constants_match_linux_x86_64() {
    use edge_libos::sys::time::{NR_CLOCK_GETTIME, NR_GETTIMEOFDAY, NR_NANOSLEEP};
    assert_eq!(NR_CLOCK_GETTIME, 228);
    assert_eq!(NR_GETTIMEOFDAY, 96);
    assert_eq!(NR_NANOSLEEP, 35);
    assert_eq!(CLOCK_REALTIME, 0);
    assert_eq!(CLOCK_MONOTONIC, 1);
}
