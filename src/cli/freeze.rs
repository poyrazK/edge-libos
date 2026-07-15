//! `edge-cli freeze <wasm> [--] [args...] --out <path>`.
//!
//! P2-D3.5: replaces the D3.3 stub with the full freeze flow.
//!
//! Drive the guest until at least one socket listener is materialized,
//! rewrite the ephemeral-port drift into the kernel state, then write
//! a postcard snapshot via `try_to_snapshot` + `write_snapshot_file`.
//!
//! Quiescence: poll every 10 ms for up to 10 s for `gs.listener.is_some()`
//! (NOT `is_listening()` — that fires at `listen()` time, before the
//! kernel-assigned port is known). Once materialized, capture
//! `listener.local_addr()?.port()` and overwrite `SocketInner.bound.port`
//! for any IPv4 listener with `port == 0`. This is the ephemeral-port-drift
//! fix: without it, snapshots taken from `bind(0.0.0.0:0)` would record
//! `bound.port = 0` and `apply_snapshot` would bind a DIFFERENT ephemeral
//! port than the snapshot says, breaking `serve`.

use std::path::PathBuf;
use std::time::Duration;

use wasmtime::Linker;

use crate::cli::error::{CliError, CliResult};
use crate::cli::util::call_start;
use crate::host::{add_to_linker, build_engine, build_store};
use crate::kernel::Kernel;
use crate::snapshot::{try_to_snapshot, write_snapshot_file, KernelSnapshot};

/// Entry point for `edge-cli freeze`. Argv layout:
///
/// - `--out <path>` (required): destination snapshot path.
/// - Positional wasm path (required).
/// - Optional `--` separator; the rest become the guest argv
///   (forwarded to `Kernel::new`).
pub async fn run_main(args: &[String]) -> CliResult<i32> {
    let mut out: Option<PathBuf> = None;
    let mut positional: Vec<String> = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--out" {
            let p = it.next().ok_or_else(|| {
                CliError::Args("freeze: --out requires a path argument".to_string())
            })?;
            out = Some(PathBuf::from(p));
        } else {
            positional.push(a.clone());
        }
    }
    let out = out.ok_or_else(|| {
        CliError::Args("usage: edge-cli freeze <wasm> [--] [args...] --out <path>".to_string())
    })?;
    if positional.is_empty() {
        return Err(CliError::Args(
            "usage: edge-cli freeze <wasm> [--] [args...] --out <path>".to_string(),
        ));
    }
    let wasm_path = positional[0].clone();
    let guest_args: Vec<String> = if positional.get(1).map(String::as_str) == Some("--") {
        positional[2..].to_vec()
    } else {
        positional[1..].to_vec()
    };

    let snap = freeze_snapshot(&wasm_path, &guest_args).await?;
    write_snapshot_file(&out, &snap)?;
    eprintln!(
        "edge-cli freeze: wrote {} ({} pages, {} fds)",
        out.display(),
        snap.pages.len(),
        snap.fds.entries.len()
    );
    Ok(0)
}

/// Instantiate + drive the guest + capture the snapshot. Split out so
/// the in-module tests can exercise argv flow without touching the FS.
async fn freeze_snapshot(wasm_path: &str, guest_args: &[String]) -> CliResult<KernelSnapshot> {
    let engine = build_engine()?;
    let mut linker: Linker<Kernel> = Linker::new(&engine);
    add_to_linker(&mut linker)?;

    let kernel = Kernel::new(guest_args.to_vec(), vec![]);
    let mut store = build_store(&engine, kernel);

    let bytes = std::fs::read(wasm_path)
        .map_err(|e| CliError::Args(format!("freeze: reading {wasm_path}: {e}")))?;
    // P3-D3.5-followup-1 (ADR 0005): SHA-256 the wasm bytes once so
    // `edge-cli serve` can refuse to apply the resulting snapshot
    // onto a mismatched wasm. Hash covers the raw file bytes —
    // for raw `.wasm` files this is the bytes `Module::new` parses;
    // for precompiled wasmtime artifacts (the `Module::deserialize`
    // branch below) this is the bytes the serve side will also
    // deserialize. Same bytes in, same hash out.
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let module_sha256: [u8; 32] = hasher.finalize().into();
    let module = if bytes.len() >= 4 && &bytes[0..4] == b"\0asm" {
        wasmtime::Module::new(&engine, &bytes)?
    } else {
        // SAFETY: callers accept `Module::deserialize` for precompiled
        // artifacts; same precondition as the deleted edge-python driver.
        unsafe { wasmtime::Module::deserialize(&engine, &bytes) }?
    };
    let instance = linker.instantiate_async(&mut store, &module).await?;
    if let Some(mem) = instance.get_memory(&mut store, "memory") {
        store.data_mut().attach_memory(mem);
    }

    // Drive the guest with a bounded timeout. The guest parks in
    // accept4 (or epoll_wait) on a real server; we take the snapshot
    // either when the guest returns (short-lived guest) or after the
    // timeout (server-style guest). `build_kernel_snapshot` does the
    // ephemeral-port-drift fix inline (see `src/snapshot.rs:561`),
    // rewriting `bound.port` to the materialized listener's
    // `local_addr().port()` if `bound.port == 0`.
    //
    // We can't poll kernel state mid-_start because `call_start` holds
    // an exclusive borrow of `Store` for the lifetime of the future —
    // see the comment at `src/cli/util.rs`. Doing the rewrite inside
    // `build_kernel_snapshot` (which holds the per-socket mutex briefly
    // and never across `.await`) is the clean place to observe the
    // materialized port without racing the guest task.
    let timeout = Duration::from_secs(10);
    // Both arms (guest returned vs. outer timeout fired on a parked
    // server-style guest) want the same snapshot of the live store, so
    // we drop the timeout future's result and unconditionally snap.
    let _ = tokio::time::timeout(timeout, call_start(&instance, &mut store)).await;
    let mut snap = try_to_snapshot(store.data(), &store)?;
    // Embed the freeze-side wasm hash on the snapshot. `try_to_snapshot`
    // builds with module_sha256 = [0u8; 32] because the guest-driven
    // `NR_SNAPSHOT` path can't supply one — we overwrite here on the
    // CLI path. (See ADR 0005 D5 design rationale.)
    snap.module_sha256 = module_sha256;
    Ok(snap)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    /// Minimal WAT that calls `exit(0)` immediately on `_start` —
    /// drives the freeze timeout down to the natural exit path
    /// rather than the 10 s server-style fallback. Imports
    /// `kernel.syscall` so the linker can satisfy the import.
    const EXIT_FAST_WAT: &str = r#"
        (module
          (import "kernel" "syscall"
            (func $syscall (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
          (memory (export "memory") 1)
          (func (export "_start") (result i64)
            ;; exit(0) → trap-as-exit, caught by `call_start`.
            (call $syscall
              (i64.const 60) (i64.const 0)
              (i64.const 0) (i64.const 0)
              (i64.const 0) (i64.const 0) (i64.const 0))))
    "#;

    #[test]
    fn argv_requires_out_path() {
        let r = rt();
        let err = r.block_on(run_main(&["foo.wasm".into()])).unwrap_err();
        assert!(matches!(err, CliError::Args(_)), "got {err:?}");
    }

    #[test]
    fn argv_requires_wasm_path() {
        let r = rt();
        let err = r
            .block_on(run_main(&["--out".into(), "/tmp/x".into()]))
            .unwrap_err();
        assert!(matches!(err, CliError::Args(_)), "got {err:?}");
    }

    #[test]
    fn out_flag_without_value_is_args_error() {
        let r = rt();
        let err = r.block_on(run_main(&["--out".into()])).unwrap_err();
        assert!(matches!(err, CliError::Args(_)), "got {err:?}");
    }

    /// P3-D3.5-followup-1 / ADR 0005: `freeze_snapshot` embeds the
    /// SHA-256 of the wasm file bytes onto the returned
    /// `KernelSnapshot.module_sha256` so a serve-side mismatch
    /// becomes a hard error. Use a tmpfile to drive the
    /// file-path entry point — no FS mocking needed.
    #[tokio::test(flavor = "current_thread")]
    async fn freeze_writes_module_sha256_to_snapshot() {
        let wasm_bytes = wat::parse_str(EXIT_FAST_WAT).expect("compile WAT");
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(&wasm_bytes);
        let expected: [u8; 32] = hasher.finalize().into();

        let dir = tempdir_in_target();
        let wasm_path = dir.join("guest.wasm");
        std::fs::write(&wasm_path, &wasm_bytes).expect("write wasm");

        let snap = freeze_snapshot(wasm_path.to_str().expect("utf8 path"), &[])
            .await
            .expect("freeze_snapshot must succeed on a fast-exit guest");
        assert_eq!(
            snap.module_sha256, expected,
            "freeze must embed SHA-256 of the wasm bytes"
        );
        // Sanity: the fixture really did `exit(0)` so the snapshot
        // captured a deterministic post-_start state (no listener,
        // just the kernel seed + 1 page of linear memory).
        assert_eq!(snap.exit_code, Some(crate::snapshot::endian::LeI32(0)));
    }

    /// Minimal `tempfile`-style helper scoped to this test module —
    /// returns a fresh dir under `target/ci/freeze-hash-tests-<pid>`
    /// that the caller can drop on test exit. Avoids pulling the
    /// `tempfile` crate as a direct dep just for two tests.
    fn tempdir_in_target() -> std::path::PathBuf {
        let pid = std::process::id();
        let dir = std::path::PathBuf::from(format!("target/ci/freeze-hash-tests-{pid}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create tempdir");
        dir
    }
}
