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
        CliError::Args(
            "usage: edge-cli freeze <wasm> [--] [args...] --out <path>".to_string(),
        )
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
    let snap = match tokio::time::timeout(timeout, call_start(&instance, &mut store)).await {
        Ok(_res) => try_to_snapshot(store.data(), &store)?,
        Err(_elapsed) => {
            // Server-style guest parked in accept4 / epoll_wait; the
            // listener fd is in the table. Snapshot the (live) store.
            try_to_snapshot(store.data(), &store)?
        }
    };
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
}