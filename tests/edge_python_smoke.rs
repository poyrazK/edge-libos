//! Smoke test for the edge-python driver.
//!
//! Compiles a tiny wasm that writes "hello\n" to fd=1 (stdout) and
//! exits with code 42, then verifies edge-python propagates both.
//!
//! Each test wasm calls `kernel.syscall` (which returns i64) and must
//! `drop` the result before end-of-block since `_start` is `void`.

mod common;

use std::process::Command;

use anyhow::Result;

#[test]
fn edge_python_propagates_stdout_and_exit_code() -> Result<()> {
    // Minimal guest: writes "hello\n" via NR_WRITE, then exits with 42.
    let wat = r#"
        (module
          (import "kernel" "syscall"
            (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
          (memory (export "memory") 1)
          (data (i32.const 4096) "hello\n")
          (func (export "_start")
            (drop (call $syscall
              (i64.const 1) (i64.const 1)
              (i64.const 4096) (i64.const 6)
              (i64.const 0) (i64.const 0) (i64.const 0)))
            (drop (call $syscall
              (i64.const 60) (i64.const 42)
              (i64.const 0) (i64.const 0)
              (i64.const 0) (i64.const 0) (i64.const 0)))
          )
        )
    "#;
    let bytes = wat::parse_str(wat).expect("compile wat");
    let tmp = tempfile::tempdir()?;
    let wasm_path = tmp.path().join("guest.wasm");
    std::fs::write(&wasm_path, &bytes)?;

    let bin = env!("CARGO_BIN_EXE_edge-python");
    let output = Command::new(bin)
        .arg(wasm_path.to_str().unwrap())
        .output()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(
        output.status.code(),
        Some(42),
        "expected exit code 42, got {:?}; stderr={}",
        output.status.code(),
        stderr
    );
    assert!(
        stdout.contains("hello"),
        "expected 'hello' in stdout, got: {stdout:?}"
    );
    Ok(())
}

#[test]
fn edge_python_drains_stderr() -> Result<()> {
    let wat = r#"
        (module
          (import "kernel" "syscall"
            (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
          (memory (export "memory") 1)
          (data (i32.const 4096) "warn\n")
          (func (export "_start")
            (drop (call $syscall
              (i64.const 1) (i64.const 2)
              (i64.const 4096) (i64.const 5)
              (i64.const 0) (i64.const 0) (i64.const 0)))
            (drop (call $syscall
              (i64.const 60) (i64.const 0)
              (i64.const 0) (i64.const 0)
              (i64.const 0) (i64.const 0) (i64.const 0)))
          )
        )
    "#;
    let bytes = wat::parse_str(wat).expect("compile wat");
    let tmp = tempfile::tempdir()?;
    let wasm_path = tmp.path().join("guest.wasm");
    std::fs::write(&wasm_path, &bytes)?;

    let bin = env!("CARGO_BIN_EXE_edge-python");
    let output = Command::new(bin)
        .arg(wasm_path.to_str().unwrap())
        .output()?;
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(
        output.status.code(),
        Some(0),
        "expected exit code 0; stderr={stderr}"
    );
    assert!(
        stderr.contains("warn"),
        "expected 'warn' on stderr, got: {stderr:?}"
    );
    Ok(())
}
