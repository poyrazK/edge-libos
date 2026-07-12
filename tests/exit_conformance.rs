//! exit / exit_group conformance.

mod common;

use anyhow::Result;

use edge_libos::Kernel;

const NR_EXIT: u32 = 60;
const NR_EXIT_GROUP: u32 = 231;

/// WAT: call exit(N) then "return N" (which never happens). After the
/// call returns (it does, because we don't trap), `kernel.exit_code`
/// must be Some(N).
const EXIT_WAT: &str = r#"
    (module
      (import "kernel" "syscall"
        (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
      (memory (export "memory") 1)
      (func (export "go") (param $code i64) (result i64)
        (drop
          (call $syscall
            (i64.const 60)
            (local.get $code)
            (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0)))
        (local.get $code)))
"#;

const EXIT_GROUP_WAT: &str = r#"
    (module
      (import "kernel" "syscall"
        (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
      (memory (export "memory") 1)
      (func (export "go") (param $code i64) (result i64)
        (drop
          (call $syscall
            (i64.const 231)
            (local.get $code)
            (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0)))
        (local.get $code)))
"#;

fn block_on<F: std::future::Future>(f: F) -> F::Output {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio current_thread runtime");
    rt.block_on(f)
}

async fn run_with_code(
    engine: &wasmtime::Engine,
    linker: &wasmtime::Linker<Kernel>,
    wat: &str,
    fn_name: &str,
    code: i64,
) -> Result<(i64, Option<i32>)> {
    let module = common::compile_wat(engine, wat)?;
    let (mut store, instance) = common::instantiate_async(engine, linker, &module).await?;
    let f = instance.get_typed_func::<(i64,), i64>(&mut store, fn_name)?;
    let ret = f.call_async(&mut store, (code,)).await?;
    let exit_code = store.data().exit_code;
    Ok((ret, exit_code))
}

#[test]
fn exit_records_code_in_kernel() -> Result<()> {
    let (engine, linker) = common::engine_and_linker()?;
    let (ret, exit_code) = block_on(run_with_code(
        &engine, &linker, EXIT_WAT, "go", 42,
    ))?;
    assert_eq!(ret, 42, "exit should return 0; code is for the kernel");
    assert_eq!(exit_code, Some(42), "kernel must record exit code");
    Ok(())
}

#[test]
fn exit_group_records_code_in_kernel() -> Result<()> {
    let (engine, linker) = common::engine_and_linker()?;
    let (ret, exit_code) = block_on(run_with_code(
        &engine, &linker, EXIT_GROUP_WAT, "go", 7,
    ))?;
    assert_eq!(ret, 7);
    assert_eq!(exit_code, Some(7), "kernel must record exit_group code");
    Ok(())
}

#[test]
fn exit_with_negative_code_truncates() -> Result<()> {
    // Linux exit code is `int` (32-bit). -1 → 0xFFFFFFFF as i32.
    let (engine, linker) = common::engine_and_linker()?;
    let (_ret, exit_code) = block_on(run_with_code(
        &engine, &linker, EXIT_WAT, "go", -1,
    ))?;
    assert_eq!(exit_code, Some(-1), "negative exit code is passed through");
    Ok(())
}

#[test]
fn nr_constants_match_linux_x86_64() {
    assert_eq!(NR_EXIT, 60);
    assert_eq!(NR_EXIT_GROUP, 231);
}
