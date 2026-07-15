//! `edge-cli serve <snap> <wasm> [--port <p>] [--] [--] [--]`.
//!
//! P2-D3.5: replaces the D3.3 stub with the full serve flow.
//!
//! Reads a snapshot file, instantiates the matching wasm module, then
//! applies the snapshot in three steps (ADR 0004 §2, §3):
//!
//! 1. `apply_snapshot_kernel_state` — no `Store` borrow; takes
//!    `&mut Kernel`. Resets `kernel.fds = FdTable::empty()`, which
//!    wipes any pre-attached inherited listeners.
//! 2. `apply_snapshot_inherited_listeners` — re-inserts the
//!    pre-built `SharedSocket` entries returned by
//!    `Kernel::attach_inherited_listeners` earlier in the flow
//!    (so the guest's `accept4(inherited_fd, …)` finds them).
//! 3. `apply_snapshot_to_memory` — needs `&mut Store<Kernel>`; we
//!    clone the `Memory` handle out of the kernel first
//!    (`Memory` is `Copy`).
//!
//! The wasm module path is required because `KernelSnapshot` does
//! not carry module bytes (`src/snapshot.rs:158-187`).
//!
//! `--port <p>` pre-mutates `snap.fds[].kind.body.socket.bound.port`
//! for any IPv4 listener fd so the reopen path at
//! `src/snapshot.rs:1009` binds to the override port. (Re-binding
//! post-apply would require holding two listeners briefly;
//! pre-mutating is simpler.)
//!
//! `EDGE_SERVE_FD_<N>` env vars (systemd-style socket activation):
//! each variable's NAME carries the decimal fd slot the
//! inherited listener will live at — i.e. the fd number the
//! listener held when the snapshot was taken (the guest's
//! `accept4(N, …)` reads back that exact slot). The VALUE is
//! the parent's OS-level fd number. We `dup` the source fd and
//! insert the listener at the target slot. See ADR 0004 §2.
//!
//! Snapshot portability caveat (NOT addressed): serve trusts the wasm
//! path matches freeze's. If the user passes a different wasm, apply
//! will succeed but the guest will mis-execute. Future: embed a
//! module hash in `KernelSnapshot`, bump `SNAPSHOT_FORMAT_VERSION`.
//! Tracked as a follow-up.

use std::path::PathBuf;

use wasmtime::{Linker, Store};

use crate::cli::error::{CliError, CliResult};
use crate::cli::util::call_start;
use crate::fd::SockAddr;
use crate::host::{add_to_linker, build_engine, build_store};
use crate::kernel::Kernel;
use crate::snapshot::{
    apply_snapshot_inherited_listeners, apply_snapshot_kernel_state, apply_snapshot_to_memory,
    read_snapshot_file, KernelSnapshot,
};

/// Prefix for systemd-style socket activation env vars (ADR 0004
/// §2). `EDGE_SERVE_FD_<N>` carries an inherited listener: the
/// suffix `<N>` is the target fd slot (matches the snapshot's
/// recorded listener fd); the value is the parent's OS-level
/// source fd number. Multiple inherited listeners get distinct N
/// suffixes (0, 1, 2, …) so they sort into a deterministic order.
const EDGE_SERVE_FD_PREFIX: &str = "EDGE_SERVE_FD_";

/// Entry point for `edge-cli serve`. Argv layout:
///
/// - `--port <p>` (optional): override the IPv4 listener bind port.
/// - Positional `<snap> <wasm>` (both required).
///
/// Inherited fds are NOT a CLI flag — they come from the
/// `EDGE_SERVE_FD_<N>` env vars per ADR 0004 §2 (systemd-style
/// socket activation). This keeps the argv shape stable across
/// deployment orchestrators.
pub async fn run_main(args: &[String]) -> CliResult<i32> {
    let mut port_override: Option<u16> = None;
    let mut positional: Vec<String> = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--port" {
            let raw = it.next().ok_or_else(|| {
                CliError::Args("serve: --port requires a number argument".to_string())
            })?;
            let p: u16 = raw.parse().map_err(|e: std::num::ParseIntError| {
                CliError::Args(format!("serve: --port: {e}"))
            })?;
            if p == 0 {
                return Err(CliError::Args(
                    "serve: --port 0 is reserved (kernel ephemeral)".to_string(),
                ));
            }
            port_override = Some(p);
        } else {
            positional.push(a.clone());
        }
    }
    if positional.len() < 2 {
        return Err(CliError::Args(
            "usage: edge-cli serve <snap> <wasm> [--port <p>]".to_string(),
        ));
    }
    let snap_path = PathBuf::from(&positional[0]);
    let wasm_path = &positional[1];

    let mut snap = read_snapshot_file(&snap_path)?;
    if let Some(p) = port_override {
        override_snapshot_port(&mut snap, p)?;
    }
    let inherited_fds = parse_inherited_fds(std::env::vars())?;
    serve_loop(&snap, wasm_path, &inherited_fds).await
}

/// Walk an iterator of `(key, value)` env-style pairs (from
/// `std::env::vars()` in production, from a `vec![..]` in tests)
/// and collect every `EDGE_SERVE_FD_<N>` value. Each variable's
/// value is the **source fd** (the parent's OS-level fd), and
/// the env-var name's `<N>` is the **target fd slot** — the
/// kernel fd the inherited listener will live at, matching the
/// fd number recorded in the snapshot when the listener was
/// frozen. Returns a `Vec<(target_fd, source_fd)>` in ascending
/// `N` order so successive inherited listeners don't collide
/// on the N axis. Returns an empty vec if no such vars are set —
/// the guest can still run without an inherited listener.
///
/// Errors:
/// - a variable whose suffix is not a decimal `u32` → `Args`
/// - a variable whose value is not a decimal `i32` → `Args`
/// - a variable whose value is negative → `Args`
///
/// We deliberately don't error on `N` collisions or non-contiguous
/// `N`s — `attach_inherited_listeners` silently skips fds that
/// can't be dup'd or whose target slot is already occupied, and
/// the guest can tolerate missing listeners (it'll just `accept4`
/// and get `EAGAIN`/`EBADF`).
///
/// Taking an iterator (rather than reading `std::env::vars()`
/// directly) makes the function pure and testable: tests pass a
/// hand-built `Vec<(String, String)>` and avoid the multi-threaded
/// race that comes with mutating the process env.
fn parse_inherited_fds<I>(env: I) -> CliResult<Vec<(u32, i32)>>
where
    I: IntoIterator<Item = (String, String)>,
{
    let mut entries: Vec<(u32, u32, i32)> = Vec::new();
    for (key, val) in env {
        let Some(suffix) = key.strip_prefix(EDGE_SERVE_FD_PREFIX) else {
            continue;
        };
        let target_fd: u32 = suffix.parse().map_err(|e: std::num::ParseIntError| {
            CliError::Args(format!(
                "serve: {key} suffix must be a non-negative integer (target fd slot): {e}"
            ))
        })?;
        let source_fd: i32 = val.parse().map_err(|e: std::num::ParseIntError| {
            CliError::Args(format!("serve: {key}={val}: {e}"))
        })?;
        if source_fd < 0 {
            return Err(CliError::Args(format!(
                "serve: {key}={source_fd}: source fds must be non-negative"
            )));
        }
        entries.push((target_fd, target_fd, source_fd));
    }
    entries.sort_by_key(|(n, _, _)| *n);
    Ok(entries
        .into_iter()
        .map(|(_, target_fd, source_fd)| (target_fd, source_fd))
        .collect())
}

/// Walk `snap.fds.entries`; for every `Resource::Socket` with
/// `is_acceptor == true` and `bound == Some(SockAddr::V4 { port, .. })`,
/// overwrite the port. Errors if no IPv4 listener was found so the user
/// gets a clear "you can't --port this snapshot" message instead of a
/// silent no-op.
fn override_snapshot_port(snap: &mut KernelSnapshot, port: u16) -> CliResult<()> {
    let mut rewrote = false;
    for entry in &mut snap.fds.entries {
        let Some(sock) = (entry.kind.kind == crate::snapshot::ResourceKind::Socket)
            .then_some(entry.kind.body.socket.as_mut())
            .flatten()
        else {
            continue;
        };
        let Some(SockAddr::V4 { port: p, .. }) = sock.bound.as_mut() else {
            continue;
        };
        if sock.is_acceptor {
            *p = port;
            rewrote = true;
        }
    }
    if !rewrote {
        return Err(CliError::Args(
            "serve: --port: snapshot has no IPv4 listener to override".to_string(),
        ));
    }
    Ok(())
}

async fn serve_loop(
    snap: &KernelSnapshot,
    wasm_path: &str,
    inherited_fds: &[(u32, i32)],
) -> CliResult<i32> {
    let engine = build_engine()?;
    let mut linker: Linker<Kernel> = Linker::new(&engine);
    add_to_linker(&mut linker)?;

    let kernel = Kernel::new(vec![], vec![]);
    let mut store: Store<Kernel> = build_store(&engine, kernel);

    let bytes = std::fs::read(wasm_path)
        .map_err(|e| CliError::Args(format!("serve: reading {wasm_path}: {e}")))?;
    let module = if bytes.len() >= 4 && &bytes[0..4] == b"\0asm" {
        wasmtime::Module::new(&engine, &bytes)?
    } else {
        unsafe { wasmtime::Module::deserialize(&engine, &bytes) }?
    };
    let instance = linker.instantiate_async(&mut store, &module).await?;
    if let Some(mem) = instance.get_memory(&mut store, "memory") {
        store.data_mut().attach_memory(mem);
    }

    // Three-step apply (ADR 0004 §2):
    //
    // 1. attach_inherited_listeners — wraps the parent's pre-opened
    //    fds in tokio::net::TcpListener + SharedSocket and inserts
    //    them at the inherited fd numbers. Returns the
    //    constructed entries so we can re-attach them after (2).
    // 2. apply_snapshot_kernel_state — resets `kernel.fds =
    //    FdTable::empty()`, which wipes the entries from (1).
    //    Inherited entries in the snapshot are short-circuited by
    //    the pre-pass `continue` so we don't try to re-bind.
    // 3. apply_snapshot_inherited_listeners — re-inserts the
    //    entries from (1) post-reset, so the guest's
    //    `accept4(inherited_fd, …)` finds them.
    // 4. apply_snapshot_to_memory — writes linear memory.
    let preattached = store.data_mut().attach_inherited_listeners(inherited_fds);
    apply_snapshot_kernel_state(snap, store.data_mut())?;
    apply_snapshot_inherited_listeners(snap, store.data_mut(), &preattached)?;
    let mem = *store
        .data()
        .memory()
        .map_err(|_| CliError::Args("serve: no memory after instantiate".to_string()))?;
    apply_snapshot_to_memory(snap, mem, &mut store)?;

    // Respawn the guest; it picks up where the snapshot left off.
    let _ = call_start(&instance, &mut store).await;
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::{
        endian::{LeI32, LeU32, LeU64},
        FdEntrySnapshot, FdSnapshot, ResourceBody, ResourceKind, ResourceSnapshot, SocketSnapshot,
    };

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    fn synth_snap_with_v4_listener(port: u16) -> KernelSnapshot {
        let sock = SocketSnapshot {
            sock_kind: crate::fd::SocketKind::Stream,
            nonblock: false,
            bound: Some(SockAddr::V4 {
                port,
                addr: [127, 0, 0, 1],
            }),
            listen_backlog: None,
            so_reuseaddr: true,
            so_keepalive: false,
            tcp_nodelay: false,
            peer_addr_present: false,
            last_error: LeI32::default(),
            shutdown_flags: 0,
            is_acceptor: true,
            peek_buf: std::collections::VecDeque::new(),
            family_unix: false,
            unix_inner: None,
            inherited: false,
        };
        let entry = FdEntrySnapshot {
            fd: LeU32::from(4u32),
            kind: ResourceSnapshot {
                kind: ResourceKind::Socket,
                body: ResourceBody {
                    socket: Some(sock),
                    ..Default::default()
                },
            },
        };
        KernelSnapshot {
            format_version: LeU32::from(crate::snapshot::SNAPSHOT_FORMAT_VERSION),
            pages: vec![],
            fds: FdSnapshot {
                entries: vec![entry],
                next_fd: LeU32::from(5u32),
                cloexec: vec![],
            },
            mm: Default::default(),
            vfs: crate::snapshot::VfsSnapshot {
                root: std::path::PathBuf::from("/"),
                cwd: std::path::PathBuf::from("/"),
            },
            clock: crate::snapshot::ClockStateSnapshot {
                boot_monotonic_ns: LeU64::default(),
            },
            brk: LeU32::default(),
            args: vec![],
            env: vec![],
            rng_seed: [0u8; 32],
            signals: Default::default(),
            exit_code: None,
            comm: [0u8; 16],
            futex_table: vec![],
            cpu_ns: LeU64::default(),
        }
    }

    /// RAII guard that unsets an env var on drop — used only by
    /// the legacy `parse_inherited_fds_returns_empty_when_no_env`
    /// test that exercises the real `std::env::vars()` path. The
    /// other tests use the pure-iterator form and don't need it.
    #[allow(dead_code)]
    struct EnvVarGuard {
        key: &'static str,
        prev: Option<String>,
    }
    impl EnvVarGuard {
        #[allow(dead_code)]
        fn set(key: &'static str, val: &str) -> Self {
            let prev = std::env::var(key).ok();
            // SAFETY: set_var / remove_var are unsafe in recent
            // Rust but are the only way to manipulate env in a
            // multi-threaded test. Tests in this module are not
            // multi-threaded (current_thread runtime) so this is
            // safe.
            unsafe { std::env::set_var(key, val) };
            Self { key, prev }
        }
    }
    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            unsafe {
                match self.prev.as_ref() {
                    Some(v) => std::env::set_var(self.key, v),
                    None => std::env::remove_var(self.key),
                }
            }
        }
    }

    #[test]
    fn argv_requires_both_paths() {
        let r = rt();
        let err = r.block_on(run_main(&["/tmp/x.snap".into()])).unwrap_err();
        assert!(matches!(err, CliError::Args(_)), "got {err:?}");
    }

    #[test]
    fn rejects_port_zero() {
        let r = rt();
        let err = r
            .block_on(run_main(&[
                "/tmp/x.snap".into(),
                "foo.wasm".into(),
                "--port".into(),
                "0".into(),
            ]))
            .unwrap_err();
        assert!(matches!(err, CliError::Args(_)), "got {err:?}");
    }

    #[test]
    fn rejects_non_numeric_port() {
        let r = rt();
        let err = r
            .block_on(run_main(&[
                "/tmp/x.snap".into(),
                "foo.wasm".into(),
                "--port".into(),
                "abc".into(),
            ]))
            .unwrap_err();
        assert!(matches!(err, CliError::Args(_)), "got {err:?}");
    }

    #[test]
    fn override_snapshot_port_writes_first_v4_listener() {
        let mut snap = synth_snap_with_v4_listener(0);
        override_snapshot_port(&mut snap, 19090).expect("override");
        let sock = snap.fds.entries[0]
            .kind
            .body
            .socket
            .as_ref()
            .expect("socket present");
        match sock.bound.as_ref().expect("bound present") {
            SockAddr::V4 { port, .. } => assert_eq!(*port, 19090),
            other => panic!("expected V4, got {other:?}"),
        }
    }

    #[test]
    fn override_snapshot_port_errors_when_no_v4_listener() {
        let mut snap = synth_snap_with_v4_listener(8080);
        // Force is_acceptor=false so the listener filter rejects it.
        snap.fds.entries[0]
            .kind
            .body
            .socket
            .as_mut()
            .expect("socket present")
            .is_acceptor = false;
        let err = override_snapshot_port(&mut snap, 19090).unwrap_err();
        assert!(matches!(err, CliError::Args(_)), "got {err:?}");
    }

    #[test]
    fn parse_inherited_fds_returns_empty_when_no_env() {
        // Pure iterator form — no env mutation, no race.
        let fds = parse_inherited_fds(std::iter::empty()).expect("parse");
        assert!(fds.is_empty());
    }

    #[test]
    fn parse_inherited_fds_skips_unrelated_vars() {
        let env = vec![
            ("PATH".to_string(), "/usr/bin".to_string()),
            ("FOO".to_string(), "bar".to_string()),
        ];
        let fds = parse_inherited_fds(env).expect("parse");
        assert!(fds.is_empty());
    }

    #[test]
    fn parse_inherited_fds_collects_and_sorts_by_n() {
        // Out-of-order keys to exercise the sort_by_key path.
        // Each tuple is (target_fd, source_fd); target_fd
        // comes from the suffix N, source_fd from the value.
        let env = vec![
            ("EDGE_SERVE_FD_2".to_string(), "42".to_string()),
            ("EDGE_SERVE_FD_0".to_string(), "10".to_string()),
            ("EDGE_SERVE_FD_1".to_string(), "20".to_string()),
        ];
        let fds = parse_inherited_fds(env).expect("parse");
        assert_eq!(fds, vec![(0, 10), (1, 20), (2, 42)], "sorted by N asc");
    }

    #[test]
    fn parse_inherited_fds_rejects_non_numeric_suffix() {
        let env = vec![("EDGE_SERVE_FD_xyz".to_string(), "10".to_string())];
        let err = parse_inherited_fds(env).unwrap_err();
        assert!(matches!(err, CliError::Args(_)), "got {err:?}");
    }

    #[test]
    fn parse_inherited_fds_rejects_negative_fd() {
        let env = vec![("EDGE_SERVE_FD_0".to_string(), "-1".to_string())];
        let err = parse_inherited_fds(env).unwrap_err();
        assert!(matches!(err, CliError::Args(_)), "got {err:?}");
    }

    #[test]
    fn parse_inherited_fds_rejects_non_numeric_value() {
        let env = vec![("EDGE_SERVE_FD_0".to_string(), "abc".to_string())];
        let err = parse_inherited_fds(env).unwrap_err();
        assert!(matches!(err, CliError::Args(_)), "got {err:?}");
    }
}
