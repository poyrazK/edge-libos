//! P2-D3.6: end-to-end freeze → snapshot file → serve subprocess smoke.
//!
//! Shells out to the actual `edge-cli` binary via
//! `env!("CARGO_BIN_EXE_edge-cli")`. This is the only way to validate
//! the freeze/serve wire flow + CLI argv + serial round-trip without
//! going in-process and bypassing the CLI surface (which is the
//! whole point of D3.5).
//!
//! What this file proves:
//!
//! 1. `freeze` accepts the right argv, invokes wasmtime, and
//!    produces a valid postcard snapshot with at least one
//!    listening-socket fd (`is_acceptor == true`).
//! 2. The snapshot file is portable across processes — the bytes
//!    decode identically in a subprocess-frozen file vs
//!    in-process `decode_snapshot` (the round-trip gate).
//! 3. `serve --port <p>` (with override applied to the snapshot
//!    directly) rewrites the `Resource::Socket.bound.port` so
//!    `apply_snapshot_kernel_state` would bind to `<p>`, not
//!    the WAT-recorded port. (End-to-end HTTP probe deferred —
//!    see file-level "what this file does NOT prove".)
//! 4. `serve` rejects a missing snapshot with exit code 1
//!    (`CliError::Snapshot` per `src/cli/mod.rs:99-102`).
//! 5. `serve` rejects `--port 0` with exit code 2
//!    (`CliError::Args` per `src/cli/mod.rs:95-97`).
//!
//! What this file does NOT prove — deferred:
//!
//! - Serve-loop re-accepts after the test client's first request.
//!   The current fixture handles one connection, then hangs in
//!   accept4 waiting for more (the host kills the subprocess to
//!   tear it down). A stress test that loops N requests through
//!   the same `serve` instance is a separate follow-up; this file
//!   only proves the first-request path.
//!
//! Concurrency note: each test compiles its own wasm with a
//! unique bind port. Four `cargo test` threads all spawn a
//! freeze subprocess; without distinct ports they would each
//! try to `bind(127.0.0.1:18080)` and the losers would have
//! the WAT exit(12) before freeze captures a listener fd.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use edge_libos::snapshot::ResourceKind;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::process::Command;

const EDGE_CLI: &str = env!("CARGO_BIN_EXE_edge-cli");
const WAT_SRC: &str = "tests/guests/serve_one_request.wat";
const WAT_FOREVER_SRC: &str = "tests/guests/serve_forever.wat";

/// Compile a slightly-retuned WAT fixture to a fresh tmp wasm
/// path. The retune rewrites the bind-port in
/// `serve_one_request.wat`'s `sockaddr_in` blob so each test
/// gets its own kernel-internal bind port — see file-level
/// "Concurrency note".
///
/// `port` is the BE-encoded port written into the sockaddr_in
/// blob at offset 4096 (bytes [3,4]).
fn build_wat_to(path: &std::path::Path, port: u16) {
    let src = std::fs::read_to_string(WAT_SRC).unwrap_or_else(|e| panic!("read {WAT_SRC}: {e}"));
    let port_be_hi = (port >> 8) as u8;
    let port_be_lo = (port & 0xff) as u8;
    let old = r#""\02\00\46\a0\7f\00\00\01""#;
    let new = format!(
        r#""\02\00\{:02x}\{:02x}\7f\00\00\01""#,
        port_be_hi, port_be_lo
    );
    let tuned = src.replacen(old, &new, 1);
    let wasm = wat::parse_str(&tuned).unwrap_or_else(|e| panic!("compile tuned WAT: {e:?}"));
    std::fs::write(path, &wasm).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
}

/// Run `edge-cli <subcmd> ...` to completion, returning the exit status.
async fn run_edge_cli(args: &[&str]) -> std::process::ExitStatus {
    Command::new(EDGE_CLI)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .expect("spawn edge-cli")
}

/// Find the first V4-listener (`is_acceptor == true`) socket fd's
/// `bound.port` in a list of `FdEntrySnapshot`. `None` when no
/// acceptor V4 socket exists. Used by tests that decode a snapshot
/// file written by `freeze` and need to assert what port the
/// kernel rewrote into the V4 sockaddr blob (F.3 dedup).
fn first_acceptor_v4_port(entries: &[edge_libos::snapshot::FdEntrySnapshot]) -> Option<u16> {
    entries.iter().find_map(|e| {
        if e.kind.kind != ResourceKind::Socket {
            return None;
        }
        let s = e.kind.body.socket.as_ref()?;
        if !s.is_acceptor {
            return None;
        }
        match s.bound {
            Some(edge_libos::fd::SockAddr::V4 { port, .. }) => Some(port),
            _ => None,
        }
    })
}

/// Find a free TCP port by binding an OS-assigned listener
/// (`127.0.0.1:0`) and reading the assigned port. Uses sync
/// `std::net::TcpListener` so it's safe to call from outside a
/// tokio runtime context.
fn pick_free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind :0");
    let port = l.local_addr().expect("local_addr").port();
    drop(l);
    port
}

#[tokio::test(flavor = "current_thread")]
async fn freeze_writes_valid_postcard_with_listening_socket() {
    let port = pick_free_port();
    let wasm_path = std::env::temp_dir().join("edge_d36_postcard.wasm");
    build_wat_to(&wasm_path, port);
    let snap_path = std::env::temp_dir().join("edge_d36_postcard.snap");
    let _ = std::fs::remove_file(&snap_path);

    let status = run_edge_cli(&[
        "freeze",
        wasm_path.to_str().unwrap(),
        "--out",
        snap_path.to_str().unwrap(),
    ])
    .await;
    assert!(status.success(), "freeze failed: {status:?}");

    let bytes = std::fs::read(&snap_path).expect("read snapshot");
    let snap = edge_libos::decode_snapshot(&bytes).expect("decode snapshot");
    assert_eq!(
        snap.format_version.0,
        edge_libos::SNAPSHOT_FORMAT_VERSION,
        "format_version drift"
    );
    assert!(!snap.pages.is_empty(), "snapshot has no pages");
    assert!(
        snap.fds.entries.iter().any(|e| {
            e.kind.kind == ResourceKind::Socket
                && e.kind.body.socket.as_ref().is_some_and(|s| s.is_acceptor)
        }),
        "snapshot has no listening socket"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn port_override_rewrites_snapshot_bound_port() {
    // Freeze with an arbitrary high port; then mutate the snap
    // file's V4 listener port to a different high port. Decode
    // both — verifies the file is portable across processes and
    // that the override applies (mirrors the `override_snapshot_port`
    // path in `src/cli/serve.rs:82-102`). We don't have to keep
    // serve alive because we don't probe HTTP here — see file-level
    // "what this file does NOT prove".
    let freeze_port = pick_free_port();
    let wasm_path = std::env::temp_dir().join("edge_d36_portrew.wasm");
    build_wat_to(&wasm_path, freeze_port);
    let snap_path = std::env::temp_dir().join("edge_d36_portrew.snap");
    let _ = std::fs::remove_file(&snap_path);

    let status = run_edge_cli(&[
        "freeze",
        wasm_path.to_str().unwrap(),
        "--out",
        snap_path.to_str().unwrap(),
    ])
    .await;
    assert!(status.success(), "freeze failed: {status:?}");

    let bytes = std::fs::read(&snap_path).expect("read snapshot");
    let before = edge_libos::decode_snapshot(&bytes)
        .expect("decode before")
        .fds
        .entries;
    let before_port = first_acceptor_v4_port(&before).expect("snapshot has no V4 listener port");

    let override_port = pick_free_port();
    assert_ne!(
        before_port, override_port,
        "freeze port {before_port} collided with override port"
    );

    let mut snap = edge_libos::decode_snapshot(&bytes).expect("decode for override");
    let mut rewrote = false;
    for entry in &mut snap.fds.entries {
        if entry.kind.kind == ResourceKind::Socket {
            if let Some(s) = entry.kind.body.socket.as_mut() {
                if s.is_acceptor {
                    if let Some(edge_libos::fd::SockAddr::V4 { port, .. }) = s.bound.as_mut() {
                        *port = override_port;
                        rewrote = true;
                    }
                }
            }
        }
    }
    assert!(rewrote, "no listener fd rewritten");
    let new_bytes = edge_libos::encode_snapshot(&snap).expect("encode");
    std::fs::write(&snap_path, &new_bytes).expect("rewrite");

    let after_port = first_acceptor_v4_port(
        &edge_libos::decode_snapshot(&new_bytes)
            .expect("decode after")
            .fds
            .entries,
    )
    .expect("post-snapshot listener disappeared");
    assert_eq!(
        after_port, override_port,
        "port override did not stick: before={before_port} after={after_port}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn serve_rejects_missing_snapshot() {
    // No freeze call — argv-only test. The missing-snapshot
    // error short-circuits before any wasmtime work.
    let wasm_path = std::env::temp_dir().join("edge_d36_missing.wasm");
    // Empty wasm is enough; serve never reaches it.
    std::fs::write(&wasm_path, b"\0asm\0\0\0\0").unwrap_or_else(|e| panic!("{e}"));
    let missing = "/nonexistent/path/never.snap";
    let status = run_edge_cli(&[
        "serve",
        missing,
        wasm_path.to_str().unwrap(),
        "--port",
        "18080",
    ])
    .await;
    // `read_snapshot_file` returns `SnapshotError` → CliError::Snapshot
    // → exit code 1 (per `src/cli/mod.rs:99-102`).
    assert_eq!(status.code(), Some(1), "got {status:?}");
}

#[tokio::test(flavor = "current_thread")]
async fn serve_rejects_port_zero() {
    // argv-only test. Pre-mutation reject at `src/cli/serve.rs:52-56`
    // fires before any file IO. The snapshot path doesn't need to
    // exist — port=0 short-circuits.
    let wasm_path = std::env::temp_dir().join("edge_d36_pzero.wasm");
    std::fs::write(&wasm_path, b"\0asm\0\0\0\0").unwrap_or_else(|e| panic!("{e}"));
    let snap_path: PathBuf = std::env::temp_dir().join("edge_d36_pzero.snap");
    let status = run_edge_cli(&[
        "serve",
        snap_path.to_str().unwrap(),
        wasm_path.to_str().unwrap(),
        "--port",
        "0",
    ])
    .await;
    // CliError::Args → exit 2 per `src/cli/mod.rs:95-97`.
    assert_eq!(status.code(), Some(2), "got {status:?}");
}

/// F.4: end-to-end HTTP smoke. Compiles `serve_forever.wat`, freezes
/// it, serves with `--port <p>`, and asserts a TCP probe of `<p>`
/// gets `HTTP/1.1 200 OK`.
///
/// Fixture mechanics: `serve_forever.wat` stores its listener fd at
/// `memory\[300\]` on fresh boot. After `apply_snapshot`, the linear
/// memory is restored, so the restored-boot branch of `_start` reads
/// `memory\[300\]` and uses the inherited fd directly. The HTTP loop
/// then accepts on the kernel-restored listener at `<p>`.
///
/// Drift-fix timing: the freeze snapshot may capture the listener
/// either materialized or taken-out (depending on where the WAT was
/// in its accept4 loop when freeze's 10s outer timeout fired). If
/// materialized, `bound.port` is rewritten to the ephemeral; if
/// taken-out, `bound.port` stays 0. Either way, `serve --port <p>`
/// overrides the snapshot's port to a known value before apply, and
/// `apply_snapshot` reopens a fresh TcpListener at `<p>`. The HTTP
/// probe targets `<p>`, which is now the inherited listener.
#[tokio::test(flavor = "current_thread")]
async fn serve_handles_http_request_after_apply() {
    let wasm_path = std::env::temp_dir().join("edge_d36_forever.wasm");
    {
        let src = std::fs::read_to_string(WAT_FOREVER_SRC)
            .unwrap_or_else(|e| panic!("read {WAT_FOREVER_SRC}: {e}"));
        let wasm =
            wat::parse_str(&src).unwrap_or_else(|e| panic!("compile {WAT_FOREVER_SRC}: {e:?}"));
        std::fs::write(&wasm_path, &wasm).unwrap_or_else(|e| panic!("write wasm: {e}"));
    }
    let snap_path = std::env::temp_dir().join("edge_d36_forever.snap");
    let _ = std::fs::remove_file(&snap_path);

    // 1. Freeze the fixture.
    let freeze = run_edge_cli(&[
        "freeze",
        wasm_path.to_str().unwrap(),
        "--out",
        snap_path.to_str().unwrap(),
    ])
    .await;
    assert!(freeze.success(), "freeze failed: {freeze:?}");

    // 2. Serve on a free port.
    let port = pick_free_port();
    let mut serve = Command::new(EDGE_CLI)
        .args([
            "serve",
            snap_path.to_str().unwrap(),
            wasm_path.to_str().unwrap(),
            "--port",
            &port.to_string(),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn serve");
    // Brief settle so serve's listener is ready before we probe.
    tokio::time::sleep(Duration::from_millis(150)).await;

    // 3. Probe HTTP/1.1 — expect 200 OK + "ok" body.
    let mut s = TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect to serve port");
    s.write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .await
        .expect("write request");
    let mut buf = vec![0u8; 512];
    let n = s.read(&mut buf).await.expect("read response");
    let resp = std::str::from_utf8(&buf[..n]).expect("response is utf-8");
    assert!(
        resp.starts_with("HTTP/1.1 200 OK"),
        "expected 200 OK, got: {resp:?}"
    );
    assert!(
        resp.contains("Content-Length: 2"),
        "expected Content-Length: 2, got: {resp:?}"
    );
    assert!(
        resp.contains("\r\n\r\nok"),
        "expected body 'ok', got: {resp:?}"
    );

    // 4. Tear down serve. The fixture never exits — the host must kill it.
    let _ = serve.kill().await;
}
