//! getrandom conformance.

mod common;

use anyhow::Result;

use edge_libos::Kernel;

const NR_GETRANDOM: u32 = 318;

/// WAT: request 32 bytes at offset 4096, then read them back as four
/// i64s and return `q0 ^ q1 ^ q2 ^ q3`. If the buffer was filled with
/// all-zeros we'd see 0; otherwise we get a non-zero signature.
const GETRANDOM_WAT: &str = r#"
    (module
      (import "kernel" "syscall"
        (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
      (memory (export "memory") 1)
      (func (export "fill_and_xor") (result i64)
        (local $ret i64)
        (local $q0 i64)
        (local $q1 i64)
        (local $q2 i64)
        (local $q3 i64)
        (local.set $ret
          (call $syscall
            (i64.const 318)
            (i64.const 4096)
            (i64.const 32)
            (i64.const 0)
            (i64.const 0) (i64.const 0) (i64.const 0)))
        (if (result i64) (i64.eq (local.get $ret) (i64.const 32))
          (then
            (local.set $q0 (i64.load (i32.const 4096)))
            (local.set $q1 (i64.load (i32.const 4104)))
            (local.set $q2 (i64.load (i32.const 4112)))
            (local.set $q3 (i64.load (i32.const 4120)))
            (return
              (i64.xor (i64.xor (local.get $q0) (local.get $q1))
                       (i64.xor (local.get $q2) (local.get $q3)))))
          (else (return (i64.const -1))))))
"#;

/// WAT: two consecutive getrandom calls — the outputs should not be byte-equal.
/// Returns first non-matching qword index, or -1 if all 4 matched.
const TWO_DRAWS_NOT_EQUAL_WAT: &str = r#"
    (module
      (import "kernel" "syscall"
        (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
      (memory (export "memory") 1)
      (func (export "two_draws_differ") (result i64)
        (local $i i64)
        (local $a i64)
        (local $b i64)
        (drop
          (call $syscall
            (i64.const 318) (i64.const 4096) (i64.const 32)
            (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0)))
        (drop
          (call $syscall
            (i64.const 318) (i64.const 6144) (i64.const 32)
            (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0)))
        (local.set $i (i64.const 0))
        (block $done
          (loop $loop
            (br_if $done (i64.ge_u (local.get $i) (i64.const 4)))
            (local.set $a
              (i64.load
                (i32.add (i32.const 4096)
                  (i32.wrap_i64 (i64.shl (local.get $i) (i64.const 3))))))
            (local.set $b
              (i64.load
                (i32.add (i32.const 6144)
                  (i32.wrap_i64 (i64.shl (local.get $i) (i64.const 3))))))
            (if (i64.ne (local.get $a) (local.get $b))
              (then (return (local.get $i))))
            (local.set $i (i64.add (local.get $i) (i64.const 1)))
            (br $loop)))
        (return (i64.const -1))))
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
fn getrandom_fills_32_bytes() -> Result<()> {
    let (engine, linker) = common::engine_and_linker()?;
    let xor_sig = block_on(run_noargs(&engine, &linker, GETRANDOM_WAT, "fill_and_xor"))?;
    assert_ne!(xor_sig, 0, "getrandom returned all-zero buffer (impossibly unlikely)");
    Ok(())
}

#[test]
fn getrandom_two_draws_differ() -> Result<()> {
    let (engine, linker) = common::engine_and_linker()?;
    let diff_idx = block_on(run_noargs(
        &engine,
        &linker,
        TWO_DRAWS_NOT_EQUAL_WAT,
        "two_draws_differ",
    ))?;
    assert!(
        diff_idx >= 0 && diff_idx < 4,
        "two consecutive getrandom calls must differ in at least one qword (got {diff_idx})"
    );
    Ok(())
}

#[test]
fn getrandom_rejects_negative_length() -> Result<()> {
    const WAT: &str = r#"
        (module
          (import "kernel" "syscall"
            (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
          (memory (export "memory") 1)
          (func (export "go") (result i64)
            (call $syscall
              (i64.const 318) (i64.const 4096) (i64.const -1)
              (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0))))
    "#;
    let (engine, linker) = common::engine_and_linker()?;
    let ret = block_on(run_noargs(&engine, &linker, WAT, "go"))?;
    assert_eq!(ret, -edge_libos::errno::EINVAL);
    Ok(())
}

#[test]
fn getrandom_eault_on_bad_pointer() -> Result<()> {
    const WAT: &str = r#"
        (module
          (import "kernel" "syscall"
            (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
          (memory (export "memory") 1)
          (func (export "go") (result i64)
            (call $syscall
              (i64.const 318) (i64.const 100000000) (i64.const 16)
              (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0))))
    "#;
    let (engine, linker) = common::engine_and_linker()?;
    let ret = block_on(run_noargs(&engine, &linker, WAT, "go"))?;
    assert_eq!(ret, -edge_libos::errno::EFAULT);
    Ok(())
}

#[test]
fn nr_constant_matches_linux_x86_64() {
    assert_eq!(NR_GETRANDOM, 318);
}
