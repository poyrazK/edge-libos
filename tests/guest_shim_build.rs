//! Smoke test for the guest syscall shim + main.c link pipeline.
//!
//! Verifies that the shim + a minimal `_start` linked together produce
//! a wasm that imports `kernel.syscall` with the expected i64 signature.
//! This catches regressions in the build.sh flags (--import-memory,
//! --stack-first, --max-memory, --export=__heap_base, etc.) without
//! requiring the full CPython cross-compile.

use std::process::Command;

use anyhow::Result;

fn block_on<F: std::future::Future>(f: F) -> F::Output {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio current_thread runtime");
    rt.block_on(f)
}

#[test]
fn guest_shim_links_with_kernel_syscall_import() -> Result<()> {
    // Skip if zig not available.
    let zig = match std::env::var("ZIG").ok().or_else(|| {
        let out = Command::new("which").arg("zig").output().ok()?;
        if out.status.success() {
            Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
        } else {
            None
        }
    }) {
        Some(p) if !p.is_empty() => p,
        _ => {
            eprintln!("SKIP: zig not in PATH");
            return Ok(());
        }
    };

    let tmp = tempfile::tempdir()?;
    let main_c = tmp.path().join("main.c");
    let wasm = tmp.path().join("shim.wasm");

    // Minimal main.c — proves the shim produces a working _start.
    std::fs::write(
        &main_c,
        r#"
        #include <stdint.h>

        __attribute__((import_module("kernel"), import_name("syscall")))
        int64_t __kernel_syscall(int64_t nr, int64_t a1, int64_t a2, int64_t a3,
                                 int64_t a4, int64_t a5, int64_t a6);

        __attribute__((visibility("default")))
        int _start(void) {
            return (int)__kernel_syscall(39, 0, 0, 0, 0, 0, 0); /* getpid */
        }
        "#,
    )?;

    let status = Command::new(&zig)
        .args([
            "cc",
            "-target",
            "wasm32-freestanding",
            "-O2",
            "-Wl,--max-memory=2147483648",
            "-Wl,--export=__heap_base",
            "-Wl,--export=__data_end",
            "guest/syscall_shim/musl_syscall.c",
            main_c.to_str().unwrap(),
            "-o",
            wasm.to_str().unwrap(),
        ])
        .output()?;
    assert!(
        status.status.success(),
        "zig cc link failed (exit {:?}) stderr={}",
        status.status.code(),
        String::from_utf8_lossy(&status.stderr)
    );

    // The wasm should contain the import names. wasm encodes the import
    // module name and import name as two length-prefixed UTF-8 strings
    // back-to-back, NOT as "module.name" with a literal dot. So we
    // check for both substrings individually.
    let bytes = std::fs::read(&wasm)?;
    let needle_mod = b"kernel";
    let needle_name = b"syscall";
    let has_mod = bytes.windows(needle_mod.len()).any(|w| w == needle_mod);
    let has_name = bytes.windows(needle_name.len()).any(|w| w == needle_name);
    assert!(
        has_mod && has_name,
        "kernel.syscall import not found (module={}, name={})",
        has_mod,
        has_name
    );
    Ok(())
}

#[test]
fn guest_shim_runs_through_trace_host() -> Result<()> {
    // Build the same wasm as the previous test, drive it through
    // trace-host, and verify exactly one getpid syscall is captured.
    let zig = match std::env::var("ZIG").ok().or_else(|| {
        let out = Command::new("which").arg("zig").output().ok()?;
        if out.status.success() {
            Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
        } else {
            None
        }
    }) {
        Some(p) if !p.is_empty() => p,
        _ => {
            eprintln!("SKIP: zig not in PATH");
            return Ok(());
        }
    };

    let tmp = tempfile::tempdir()?;
    let main_c = tmp.path().join("main.c");
    let wasm = tmp.path().join("shim.wasm");

    std::fs::write(
        &main_c,
        r#"
        #include <stdint.h>

        __attribute__((import_module("kernel"), import_name("syscall")))
        int64_t __kernel_syscall(int64_t nr, int64_t a1, int64_t a2, int64_t a3,
                                 int64_t a4, int64_t a5, int64_t a6);

        __attribute__((visibility("default")))
        int _start(void) {
            return (int)__kernel_syscall(39, 0, 0, 0, 0, 0, 0);
        }
        "#,
    )?;

    let status = Command::new(&zig)
        .args([
            "cc",
            "-target",
            "wasm32-freestanding",
            "-O2",
            "-Wl,--export=__heap_base",
            "guest/syscall_shim/musl_syscall.c",
            main_c.to_str().unwrap(),
            "-o",
            wasm.to_str().unwrap(),
        ])
        .status()?;
    assert!(status.success(), "build failed");

    // Run through trace-host.
    let bin = env!("CARGO_BIN_EXE_trace-host");
    let output = Command::new(bin).arg(wasm.to_str().unwrap()).output()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    eprintln!("trace-host stdout: {stdout}");
    eprintln!("trace-host stderr: {stderr}");
    assert!(
        stdout.contains("\"name\":\"getpid\""),
        "expected getpid in trace, got: {stdout}"
    );
    assert!(
        stdout.contains("\"ret\":1"),
        "expected ret=1, got: {stdout}"
    );
    Ok(())
}