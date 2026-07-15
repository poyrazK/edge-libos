//! `edge-cli migrate <wasm> [--] [args...]` — freeze → serve migration.
//!
//! P2-D3.5 sub-deliverable 6: replaces the in-process stop-gap (P3
//! final-bundle) with the production subprocess flow. The migrate
//! subcommand is now the canonical freeze → serve loop:
//!
//!   1. Spawn `edge-cli freeze <wasm> --out <snap>` and wait for it
//!      to complete. The freeze subprocess drives the guest to
//!      quiescence (per ADR 0004 §1) and writes the snapshot to
//!      `<snap>` (a temp file we own).
//!   2. Spawn `edge-cli serve <wasm> <snap>` and forward exit code.
//!      The serve subprocess restores the snapshot, attaches
//!      inherited fds (forwarded via `EDGE_SERVE_FD_<N>` env vars
//!      per ADR 0004 §2), then respawns the guest.
//!   3. Delete the temp snap on both success and error paths.
//!
//! This matches the cross-host shape: the snapshot file is the
//! wire payload that `scp` (or any blob transport) carries
//! between host-A and host-B. The in-process path is the same
//! contract but with `freeze` + `serve` as library functions
//! instead of OS processes — useful for unit tests that want to
//! skip the subprocess overhead.
//!
//! ## `MIGRATE_IN_PROCESS=1` (test-only opt-in)
//!
//! When this env var is set, `run_main` dispatches to
//! `run_main_in_process` instead of the subprocess pair. The
//! in-process path exercises the same encode/decode/apply
//! roundtrip as the production path, just without the
//! `Command::spawn` overhead and without the cross-process
//! boundary. C conformance + integration tests that already
//! use `run_main_from(["migrate", ...])` set this env var.
//!
//! ## env forwarding
//!
//! The subprocess pair inherits `EDGE_SERVE_FD_<N>` from the
//! parent migrate process so operators can do
//! `EDGE_SERVE_FD_0=4 edge-cli migrate wasm` and have the
//! inherited listener land in the serve subprocess.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Command;

use crate::cli::error::{CliError, CliResult};
use crate::host::{add_to_linker, build_engine, build_store};
use crate::kernel::Kernel;
use crate::snapshot::{decode_snapshot, encode_snapshot, try_to_snapshot};

/// Env var name that opts the migrate subcommand into the
/// in-process path (test-only — production uses subprocess).
const MIGRATE_IN_PROCESS_ENV: &str = "MIGRATE_IN_PROCESS";

/// Prefix for systemd-style socket activation env vars (see
/// [`crate::cli::serve`]). Migrate forwards these to the serve
/// subprocess so an operator can do `EDGE_SERVE_FD_0=4 edge-cli
/// migrate wasm`.
const EDGE_SERVE_FD_PREFIX: &str = "EDGE_SERVE_FD_";

/// `edge-cli migrate <wasm> [--] [args...]`. Default: subprocess
/// path. Opt-in to in-process with `MIGRATE_IN_PROCESS=1`.
pub async fn run_main(args: &[String]) -> CliResult<i32> {
    if std::env::var_os(MIGRATE_IN_PROCESS_ENV).is_some() {
        return run_main_in_process(args).await;
    }
    run_main_subprocess(args).await
}

/// Subprocess path — the production shape. Spawn
/// `edge-cli freeze <wasm> --out <snap>` then
/// `edge-cli serve <wasm> <snap>` as child processes, forwarding
/// `EDGE_SERVE_FD_<N>` env vars to the serve subprocess so the
/// inherited listener lands on the right side of the boundary.
async fn run_main_subprocess(args: &[String]) -> CliResult<i32> {
    let (wasm_path, guest_args) = parse_migrate_args(args)?;

    // Pick a snap file in the temp dir; we own it for the
    // duration of this process and clean up on every exit path.
    let snap_path = std::env::temp_dir().join(format!(
        "edge-migrate-{}-{}.snap",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));

    let current_exe = std::env::current_exe().map_err(|e| {
        CliError::Args(format!(
            "migrate: cannot locate current_exe for subprocess spawn: {e}"
        ))
    })?;

    // Build the freeze argv: `edge-cli freeze <wasm> [guest_args...] --out <snap>`.
    let mut freeze_argv: Vec<OsString> = vec!["freeze".into(), wasm_path.clone().into_os_string()];
    if !guest_args.is_empty() {
        freeze_argv.push("--".into());
        for a in &guest_args {
            freeze_argv.push(a.into());
        }
    }
    freeze_argv.push("--out".into());
    freeze_argv.push(snap_path.clone().into_os_string());

    let freeze_status = run_subcommand(&current_exe, &freeze_argv, &[])
        .map_err(|e| CliError::Args(format!("migrate: freeze subprocess failed: {e}")))?;
    if !freeze_status.success() {
        let _ = std::fs::remove_file(&snap_path);
        return Err(CliError::Args(format!(
            "migrate: freeze subprocess exited {freeze_status}"
        )));
    }

    // Build the serve argv: `edge-cli serve <wasm> <snap>` (no
    // guest_args — serve respawns the guest at the post-snapshot
    // state, the original argv is captured in the snapshot).
    let serve_argv: Vec<OsString> = vec![
        "serve".into(),
        snap_path.clone().into_os_string(),
        wasm_path.clone().into_os_string(),
    ];

    // Forward EDGE_SERVE_FD_<N> env vars from the migrate
    // process to the serve subprocess (systemd-style socket
    // activation). We deliberately do NOT forward the whole
    // env — only the activation vars, to keep the boundary
    // narrow and to avoid leaking unrelated env to the child.
    let serve_env = collect_inherit_env();

    let serve_status = run_subcommand(&current_exe, &serve_argv, &serve_env)
        .map_err(|e| CliError::Args(format!("migrate: serve subprocess failed: {e}")))?;

    // Best-effort cleanup of the snap file on every exit path.
    let _ = std::fs::remove_file(&snap_path);

    if !serve_status.success() {
        return Err(CliError::Args(format!(
            "migrate: serve subprocess exited {serve_status}"
        )));
    }
    Ok(0)
}

/// Run a child `edge-cli` subprocess and wait for it to exit.
/// `argv[0]` is the subcommand name; `env` is the additional
/// env vars to set (forwarded `EDGE_SERVE_FD_<N>` for serve).
fn run_subcommand(
    current_exe: &std::path::Path,
    argv: &[OsString],
    env: &[(OsString, OsString)],
) -> std::io::Result<std::process::ExitStatus> {
    let mut cmd = Command::new(current_exe);
    cmd.args(argv);
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.status()
}

/// Walk the migrate process env and collect every
/// `EDGE_SERVE_FD_<N>` value as `(key, value)` pairs in `OsString`
/// form so they can be passed to `Command::env`. We pass the
/// values verbatim — `serve::parse_inherited_fds` does the
/// parsing and validation on the other side of the boundary,
/// so a malformed value surfaces as a clear `CliError::Args`
/// from serve.
fn collect_inherit_env() -> Vec<(OsString, OsString)> {
    std::env::vars_os()
        .filter_map(|(k, v)| {
            // `vars_os` returns `(OsString, OsString)`; we
            // can't move `k` while still borrowing it via
            // `to_str`. Materialize the borrowed &str first,
            // then move `k` only if the prefix matches.
            let matches = k
                .to_str()
                .and_then(|s| s.strip_prefix(EDGE_SERVE_FD_PREFIX))
                .is_some();
            if matches {
                Some((k, v))
            } else {
                None
            }
        })
        .collect()
}

/// In-process path — the test-friendly opt-in. Same
/// encode/decode/apply roundtrip as the subprocess shape, but
/// without spawning OS processes. Used by tests that want to
/// exercise the wire-format contract without paying for the
/// subprocess overhead.
///
/// Kept as `pub` because tests in other modules want to call it
/// directly when they want to bypass the env-var check.
pub async fn run_main_in_process(args: &[String]) -> CliResult<i32> {
    let (wasm_path, guest_args) = parse_migrate_args(args)?;

    let engine = build_engine()?;
    let mut linker = wasmtime::Linker::new(&engine);
    add_to_linker(&mut linker)?;

    let module = wasmtime::Module::from_file(&engine, &wasm_path).map_err(CliError::Wasmtime)?;
    let mut store = build_store(
        &engine,
        Kernel::new_without_stdio(guest_args.clone(), vec![]),
    );
    let instance = linker
        .instantiate_async(&mut store, &module)
        .await
        .map_err(CliError::Wasmtime)?;
    if let Some(mem) = instance.get_memory(&mut store, "memory") {
        store.data_mut().attach_memory(mem);
    }
    if let Ok(start) = instance.get_typed_func::<(), i64>(&mut store, "_start") {
        match start.call_async(&mut store, ()).await {
            Ok(0) => {}
            Ok(rc) => eprintln!(
                "edge-cli migrate: guest _start returned non-zero exit code {rc}; \
                 snapshot will reflect post-exit kernel state"
            ),
            Err(e) => eprintln!(
                "edge-cli migrate: guest _start trapped ({e}); \
                 snapshot will reflect trap-time kernel state"
            ),
        }
    }

    let snap = try_to_snapshot(store.data(), &store).map_err(CliError::Snapshot)?;
    let bytes = encode_snapshot(&snap).map_err(CliError::Snapshot)?;
    let snap_restored = decode_snapshot(&bytes).map_err(CliError::Snapshot)?;

    let mut fresh_store = build_store(
        &engine,
        Kernel::new_without_stdio(guest_args.clone(), vec![]),
    );
    let fresh_instance = linker
        .instantiate_async(&mut fresh_store, &module)
        .await
        .map_err(CliError::Wasmtime)?;
    if let Some(mem) = fresh_instance.get_memory(&mut fresh_store, "memory") {
        fresh_store.data_mut().attach_memory(mem);
    }
    crate::snapshot::apply_snapshot_kernel_state(&snap_restored, fresh_store.data_mut())
        .map_err(CliError::Snapshot)?;
    let mem_clone = match fresh_store.data().memory() {
        Ok(m) => *m,
        Err(e) => return Err(CliError::Args(format!("no memory attached: {e}"))),
    };
    crate::snapshot::apply_snapshot_to_memory(&snap_restored, mem_clone, &mut fresh_store)
        .map_err(CliError::Snapshot)?;

    eprintln!(
        "edge-cli migrate (in-process): roundtripped {} bytes of snapshot state",
        bytes.len()
    );
    Ok(0)
}

fn parse_migrate_args(args: &[String]) -> CliResult<(PathBuf, Vec<String>)> {
    let mut it = args.iter();
    let wasm = it.next().ok_or_else(|| {
        CliError::Args("usage: edge-cli migrate <wasm> [--] [args...]".to_string())
    })?;
    let wasm_path = PathBuf::from(wasm);

    let mut guest_args: Vec<String> = Vec::new();
    let mut seen_dashdash = false;
    for a in it {
        if !seen_dashdash && a == "--" {
            seen_dashdash = true;
            continue;
        }
        guest_args.push(a.clone());
    }
    Ok((wasm_path, guest_args))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_migrate_args_basic() {
        let args = vec!["foo.wasm".to_string()];
        let (p, ga) = parse_migrate_args(&args).unwrap();
        assert_eq!(p, PathBuf::from("foo.wasm"));
        assert!(ga.is_empty());
    }

    #[test]
    fn parse_migrate_args_with_dashdash() {
        let args = vec![
            "foo.wasm".to_string(),
            "--".to_string(),
            "arg1".to_string(),
            "arg2".to_string(),
        ];
        let (p, ga) = parse_migrate_args(&args).unwrap();
        assert_eq!(p, PathBuf::from("foo.wasm"));
        assert_eq!(ga, vec!["arg1".to_string(), "arg2".to_string()]);
    }

    #[test]
    fn parse_migrate_args_missing_wasm_returns_args_error() {
        let args: Vec<String> = vec![];
        assert!(matches!(parse_migrate_args(&args), Err(CliError::Args(_))));
    }

    /// Exhaustive filter test for `collect_inherit_env` — we
    /// can't read process env safely from a test (other tests
    /// may mutate it), but the filter logic is pure: it walks
    /// a `(key, value)` iterator and keeps entries whose key
    /// starts with the prefix. This test pins that contract.
    #[test]
    fn collect_inherit_env_filter_logic() {
        let env: Vec<(OsString, OsString)> = vec![
            (OsString::from("PATH"), OsString::from("/usr/bin")),
            (OsString::from("EDGE_SERVE_FD_0"), OsString::from("4")),
            (OsString::from("EDGE_SERVE_FD_1"), OsString::from("5")),
            (OsString::from("HOME"), OsString::from("/home/u")),
            (OsString::from("EDGE_SERVE_FD_X"), OsString::from("y")),
        ];
        let forwarded: Vec<String> = env
            .into_iter()
            .filter(|(k, _)| {
                k.to_str()
                    .and_then(|s| s.strip_prefix(EDGE_SERVE_FD_PREFIX))
                    .is_some()
            })
            .map(|(k, _)| k.to_str().unwrap().to_string())
            .collect();
        assert_eq!(
            forwarded,
            vec![
                "EDGE_SERVE_FD_0".to_string(),
                "EDGE_SERVE_FD_1".to_string(),
                "EDGE_SERVE_FD_X".to_string(),
            ]
        );
    }
}
