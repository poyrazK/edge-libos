//! P2-D3.5 sub-deliverable 7 — full prod-shape e2e integration.
//!
//! The headline D3.5 integration test: freeze a server-style guest
//! on host-A, copy the snapshot to host-B, then serve on host-B
//! with a pre-opened TCP listener inherited from the parent process
//! (systemd-style `EDGE_SERVE_FD_<N>` socket activation — ADR 0004
//! §2). A real TCP client probes the inherited listener and asserts
//! the server-style guest echoes back the expected HTTP response.
//!
//! This is the only test that exercises the production wire
//! contract end-to-end:
//!   1. `edge-cli freeze <wasm> --out <snap>` produces a valid
//!      postcard snapshot.
//!   2. Snapshot is patched to mark the listener as `inherited`
//!      so the apply path takes the skip-rebuild branch (the
//!      fixture's FRESH BOOT path binds a non-inherited listener;
//!      we re-purpose the same fixture for the inherited path by
//!      flipping the flag in the snapshot after freeze).
//!   3. Parent process binds a TCP listener and exec's
//!      `edge-cli serve <wasm> <snap>` with
//!      `EDGE_SERVE_FD_<listener_fd>=<fd>`.
//!   4. Serve attaches the inherited listener as
//!      `Resource::Socket` (no re-bind), respawns the guest at
//!      the post-snapshot state, guest accepts on the inherited
//!      fd.
//!   5. TCP client probes `127.0.0.1:<p>` and asserts the
//!      expected response.

use std::os::unix::io::IntoRawFd;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use edge_libos::fd::SockAddr;
use edge_libos::snapshot::{encode_snapshot, read_snapshot_file, ResourceKind};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Compile a tiny "echo HTTP" fixture to a temp file and return
/// its path. The fixture binds a listener, accepts in a loop,
/// and writes a fixed HTTP response — same shape as
/// `tests/edge_cli_freeze_serve_smoke.rs::WAT_FOREVER_SRC`.
fn build_forever_wasm(path: &Path) {
    // Reuse the same WAT source the freeze/serve smoke
    // exercises; it stores its listener fd at memory[300] on
    // fresh boot and reads it back on restored boot. After
    // apply_snapshot restores linear memory, the restored-boot
    // branch uses the inherited fd directly — which is
    // precisely the listener we passed via EDGE_SERVE_FD_<N>.
    const SRC: &str = include_str!("../tests/guests/serve_forever.wat");
    let wasm = wat::parse_str(SRC).expect("compile serve_forever.wat");
    std::fs::write(path, &wasm).expect("write forever wasm");
}

/// Pick a free TCP port by binding on 127.0.0.1:0 and reading the
/// assigned port. Returns the port number; the listener is
/// dropped so the port is closed again. (The parent process
/// then re-binds the port for the inherited-listener test.)
///
/// Async because the caller is already inside a tokio runtime
/// (the test is `#[tokio::test(flavor = "current_thread")]`) —
/// building a second runtime would panic with "Cannot start a
/// runtime from within a runtime".
async fn pick_free_port() -> u16 {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral");
    let port = l.local_addr().expect("local_addr").port();
    drop(l);
    // Brief settle so the kernel releases the port before
    // we re-bind it in the parent shell.
    tokio::time::sleep(Duration::from_millis(20)).await;
    port
}

/// Resolve the edge-cli binary path (CARGO_BIN_EXE_edge-cli or
/// `<test_exe_dir>/../edge-cli` fallback — same heuristic as
/// `tests/migration_smoke.rs::migration_smoke_subprocess_roundtrip`).
fn edge_cli_path() -> String {
    std::env::var("CARGO_BIN_EXE_edge-cli")
        .ok()
        .or_else(|| {
            let exe = std::env::current_exe().ok()?;
            let candidate = exe.parent()?.join("..").join("edge-cli");
            if candidate.is_file() {
                Some(candidate.to_string_lossy().to_string())
            } else {
                None
            }
        })
        .expect("locate edge-cli binary")
}

/// Probe whether `port` is connectable on 127.0.0.1. Returns
/// true on success; used to verify the inherited listener is
/// actually accepting (otherwise the test could hang on a
/// dead listener).
async fn port_listening(port: u16) -> bool {
    TcpStream::connect(("127.0.0.1", port))
        .await
        .map(|_| ())
        .is_ok()
}

/// Mutate the snapshot to mark the listener socket entry as
/// `inherited: true` and rewrite its bound port to `port`.
/// This simulates the production case where the listener is
/// inherited from the parent (systemd-style socket
/// activation): in the prod shape, the snapshot was taken
/// from a kernel that had already attached an inherited
/// listener, so the entry is born with `inherited: true`.
/// For this e2e test we re-purpose the existing WAT fixture
/// (whose FRESH BOOT path binds a non-inherited listener);
/// post-processing the snapshot lets us exercise the
/// inherited-listener apply path without a separate fixture.
///
/// Returns the listener fd number so the caller can attach
/// the inherited listener at the matching fd (per ADR 0004
/// §2, `apply_snapshot_kernel_state` preserves the fd number
/// for inherited entries).
fn mark_snapshot_listener_inherited(snap_path: &Path, port: u16) -> u32 {
    let mut snap = read_snapshot_file(snap_path).expect("read snapshot");
    let entry = snap
        .fds
        .entries
        .iter_mut()
        .find(|e| e.kind.kind == ResourceKind::Socket)
        .expect("snapshot has no socket entry");
    let sock = entry
        .kind
        .body
        .socket
        .as_mut()
        .expect("socket body present");
    sock.inherited = true;
    // Rewrite the bound port so the snapshot's recorded
    // `bound.port` matches the inherited listener's actual
    // port. The apply path doesn't bind for inherited entries,
    // but downstream queries (and tests like
    // `apply_snapshot_with_inherited_listener_does_not_rebind`)
    // read `bound.port` — keeping it consistent avoids drift
    // confusion.
    sock.bound = Some(SockAddr::V4 {
        port,
        addr: [127, 0, 0, 1],
    });
    let fd_num = entry.fd.0;
    let bytes = encode_snapshot(&snap).expect("re-encode snapshot");
    std::fs::write(snap_path, &bytes).expect("write patched snapshot");
    fd_num
}

/// Full prod-shape e2e:
///   1. Compile the serve_forever fixture.
///   2. Freeze it via `edge-cli freeze`.
///   3. Patch the snapshot to mark the listener as inherited
///      (simulating the prod case where the freeze host had
///      already attached an inherited listener).
///   4. Bind a fresh listener, exec `edge-cli serve` with
///      `EDGE_SERVE_FD_<listener_fd>=<raw_fd>`.
///   5. TCP-probe the inherited listener, assert HTTP 200.
#[tokio::test(flavor = "current_thread")]
async fn freeze_then_serve_with_inherited_listener() {
    let wasm_path = std::env::temp_dir().join("edge_d35_e2e_forever.wasm");
    let snap_path = std::env::temp_dir().join("edge_d35_e2e_forever.snap");
    let _ = std::fs::remove_file(&snap_path);
    build_forever_wasm(&wasm_path);

    let cli = edge_cli_path();

    // Phase 1: freeze the fixture.
    let freeze_status = std::process::Command::new(&cli)
        .args([
            "freeze",
            wasm_path.to_str().unwrap(),
            "--out",
            snap_path.to_str().unwrap(),
        ])
        .status()
        .expect("spawn freeze");
    assert!(
        freeze_status.success(),
        "freeze exited non-zero: {freeze_status}"
    );
    assert!(
        snap_path.is_file(),
        "freeze did not write snapshot at {}",
        snap_path.display()
    );

    // Phase 2: pick a free port and patch the snapshot to
    // mark the listener as inherited (and rewrite the bound
    // port to match). The listener_fd is what the WAT
    // fixture stores at memory[300] and reads back on
    // restored boot — so the inherited listener MUST be
    // attached at the same fd number.
    let port = pick_free_port().await;
    let listener_fd = mark_snapshot_listener_inherited(&snap_path, port);

    // Phase 3: bind a fresh TCP listener for the parent
    // shell to "inherit" via EDGE_SERVE_FD_<listener_fd>.
    // Use std::net so we can convert into a raw fd without
    // depending on tokio ownership semantics — the serve
    // subprocess will dup it via
    // Kernel::attach_inherited_listeners.
    let std_listener =
        std::net::TcpListener::bind(("127.0.0.1", port)).expect("bind inherited listener");
    std_listener.set_nonblocking(false).ok();
    let raw_fd: i32 = std_listener.into_raw_fd();
    // Clear FD_CLOEXEC so the fd survives `exec` in the
    // serve subprocess. std::net::TcpListener's underlying
    // socket is created with CLOEXEC set by the kernel;
    // without clearing it here, the child process sees
    // raw_fd already closed and
    // Kernel::attach_inherited_listeners' libc::dup returns
    // -1, dropping the listener silently.
    // SAFETY: raw_fd is a valid fd we just took ownership
    // of; F_GETFD / F_SETFD with FD_CLOEXEC are the
    // documented ways to inspect and clear the flag.
    unsafe {
        let flags = libc::fcntl(raw_fd, libc::F_GETFD);
        if flags >= 0 {
            libc::fcntl(raw_fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC);
        }
    }

    // Phase 4: spawn the serve subprocess with
    // `EDGE_SERVE_FD_<listener_fd>=<raw_fd>` (ADR 0004 §2):
    // the env var NAME's suffix `<listener_fd>` becomes the
    // kernel fd target slot (must equal the snapshot's
    // recorded listener fd so the guest's `accept4(<listener_fd>)`
    // finds it after apply). The VALUE is the parent's
    // OS-level raw fd (the one we bound in Phase 3). Serve
    // reads both, dups the source fd, and inserts at the
    // target slot.
    let env_var = format!("EDGE_SERVE_FD_{listener_fd}");
    let stderr_path = std::env::temp_dir().join("edge_d35_e2e_serve.stderr");
    let _ = std::fs::remove_file(&stderr_path);
    let stderr_file = std::fs::File::create(&stderr_path).expect("create stderr file");
    let mut serve = tokio::process::Command::new(&cli)
        .args([
            "serve",
            snap_path.to_str().unwrap(),
            wasm_path.to_str().unwrap(),
        ])
        .env(&env_var, raw_fd.to_string())
        .stdout(Stdio::piped())
        .stderr(Stdio::from(stderr_file))
        .kill_on_drop(true)
        .spawn()
        .expect("spawn serve");
    // Brief settle so serve's listener is ready before we probe.
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert!(
        port_listening(port).await,
        "inherited listener at port {port} is not accepting after 250ms"
    );

    // Phase 5: probe the inherited listener. The serve
    // subprocess did NOT re-bind — it attached our raw_fd
    // directly. A successful HTTP 200 means:
    //   * freeze produced a valid snapshot
    //   * serve applied kernel state + memory
    //   * attach_inherited_listeners wrapped the fd correctly
    //   * apply_snapshot_inherited_listeners re-inserted after
    //     the kernel-state reset
    //   * the guest's restored-boot branch accepted on the
    //     inherited fd
    let mut client = TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect inherited listener");
    client
        .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .await
        .expect("write request");
    let mut buf = vec![0u8; 512];
    let n = tokio::time::timeout(Duration::from_secs(5), client.read(&mut buf))
        .await
        .expect("read timed out after 5s")
        .expect("read response");
    let resp = std::str::from_utf8(&buf[..n]).expect("utf-8 response");
    assert!(
        resp.starts_with("HTTP/1.1 200 OK"),
        "expected 200 OK from inherited listener, got: {resp:?}"
    );
    assert!(
        resp.contains("\r\n\r\nok"),
        "expected body 'ok', got: {resp:?}"
    );

    // Cleanup. The fixture never exits, so the host must kill
    // it; we also remove the snap file so successive runs don't
    // accumulate state.
    let _ = serve.kill().await;
    if let Ok(s) = std::fs::read_to_string(&stderr_path) {
        if !s.is_empty() {
            eprintln!("serve stderr:\n{s}");
        }
    }
    let _ = std::fs::remove_file(&snap_path);
    let _ = std::fs::remove_file(&stderr_path);
}

/// P3-D3.5-followup-1 / ADR 0005 — mismatch rejection on the real
/// subprocess path. Freezes with the production `serve_forever.wat`,
/// then runs `edge-cli serve` against a DIFFERENT wasm
/// (`serve_one_request.wat`). The serve subprocess MUST exit
/// non-zero before any apply step, with stderr containing
/// `module hash mismatch` (or the matching `CliError::Snapshot`
/// display). This is the headlined end-to-end repro of the
/// silent-mis-execution fix.
#[tokio::test(flavor = "current_thread")]
async fn cli_migration_e2e_rejects_mismatched_wasm() {
    // 1. Build the freeze-side wasm (`serve_forever.wat`) — same
    //    fixture the happy-path test uses — and the serve-side
    //    wasm (a deliberately different module, `serve_one_request.wat`).
    //    They have DIFFERENT byte content so SHA-256 disagrees.
    let freeze_wasm_path = std::env::temp_dir().join("edge_d35_e2e_mismatch_freeze.wasm");
    let serve_wasm_path = std::env::temp_dir().join("edge_d35_e2e_mismatch_serve.wasm");
    let snap_path = std::env::temp_dir().join("edge_d35_e2e_mismatch.snap");
    let _ = std::fs::remove_file(&freeze_wasm_path);
    let _ = std::fs::remove_file(&serve_wasm_path);
    let _ = std::fs::remove_file(&snap_path);

    let freeze_wat = wat::parse_str(include_str!("../tests/guests/serve_forever.wat"))
        .expect("compile serve_forever.wat");
    let serve_wat = wat::parse_str(include_str!("../tests/guests/serve_one_request.wat"))
        .expect("compile serve_one_request.wat");
    std::fs::write(&freeze_wasm_path, &freeze_wat).expect("write freeze wasm");
    std::fs::write(&serve_wasm_path, &serve_wat).expect("write serve wasm");

    let cli = edge_cli_path();

    // 2. Freeze the freeze-side wasm. The snapshot's
    //    `module_sha256` will be SHA-256 of `freeze_wasm_path`'s
    //    bytes — by design different from the serve-side wasm's
    //    SHA-256.
    let freeze_status = std::process::Command::new(&cli)
        .args([
            "freeze",
            freeze_wasm_path.to_str().unwrap(),
            "--out",
            snap_path.to_str().unwrap(),
        ])
        .status()
        .expect("spawn freeze");
    assert!(
        freeze_status.success(),
        "freeze must succeed (exit 0): {freeze_status}"
    );
    assert!(
        snap_path.is_file(),
        "freeze did not write snapshot at {}",
        snap_path.display()
    );

    // 3. Run serve with the MISMATCHED serve-side wasm. Capture
    //    stderr; the dispatcher's exit-1 path for
    //    `CliError::Snapshot` will fire because the verify call
    //    rejects the wasm BEFORE any apply step runs.
    let stderr_path = std::env::temp_dir().join("edge_d35_e2e_mismatch_serve.stderr");
    let _ = std::fs::remove_file(&stderr_path);
    let stderr_file = std::fs::File::create(&stderr_path).expect("create stderr file");
    let mut serve = tokio::process::Command::new(&cli)
        .args([
            "serve",
            snap_path.to_str().unwrap(),
            serve_wasm_path.to_str().unwrap(),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr_file))
        .kill_on_drop(true)
        .spawn()
        .expect("spawn serve (mismatch)");
    // The mismatch should fire synchronously inside serve_loop —
    // no listener is ever bound, so a short timeout is plenty.
    let serve_status = tokio::time::timeout(Duration::from_secs(5), serve.wait())
        .await
        .expect("serve should exit promptly on mismatch")
        .expect("serve.wait() ok");
    // Capture stderr for the diagnostic assertion below.
    let stderr = std::fs::read_to_string(&stderr_path).unwrap_or_default();

    assert!(
        !serve_status.success(),
        "serve MUST exit non-zero on wasm hash mismatch; got {:?} stderr={stderr:?}",
        serve_status.code()
    );
    assert_eq!(
        serve_status.code(),
        Some(1),
        "serve must exit with code 1 (the dispatcher maps Snapshot → exit 1)"
    );
    assert!(
        stderr.contains("module hash mismatch") || stderr.contains("ModuleHashMismatch"),
        "expected stderr to mention the module hash mismatch error; got: {stderr:?}"
    );

    // 4. Cleanup tmpfiles so successive runs don't accumulate.
    let _ = std::fs::remove_file(&freeze_wasm_path);
    let _ = std::fs::remove_file(&serve_wasm_path);
    let _ = std::fs::remove_file(&snap_path);
    let _ = std::fs::remove_file(&stderr_path);
}
