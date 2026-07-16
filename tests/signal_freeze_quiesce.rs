//! P3 / ADR 0007 §6: SIGUSR1 → freeze quiescence smoke.
//!
//! What this file proves:
//!
//! 1. `edge-cli freeze <wasm>` installs a `quiesce_notify` on the
//!    guest's `Arc<ProcessState>` and spawns a dedicated OS thread
//!    that listens for `SIGUSR1` and fires the notify
//!    (`src/cli/freeze.rs::spawn_sigusr1_listener`).
//! 2. When SIGUSR1 is delivered to the freeze subprocess while the
//!    guest is parked in a blocking syscall, freeze wakes the
//!    `call_start` future out of its `select!` arm, takes a snapshot,
//!    and exits before the 10-second outer timeout fires.
//! 3. The snapshot file is still a valid postcard (decode succeeds,
//!    `format_version == SNAPSHOT_FORMAT_VERSION`) — the quiesce
//!    path goes through the same `try_to_snapshot` as the
//!    timeout-and-exit path.
//!
//! What this file does NOT prove (deferred):
//!
//! - That the captured snapshot is *useful* — the fixture parks in
//!   `epoll_wait` without ever opening a listening socket, so the
//!   snapshot has no acceptor fd and isn't replayable. That's the
//!   same as the timeout path with this fixture; a full
//!   "SIGUSR1-snapshots-a-live-listener" smoke belongs with the
//!   freeze/serve end-to-end suite and is not required for the
//!   quiesce wiring itself.
//!
//! Concurrency note: the test grabs the freeze PID via the
//! `Command::id()` on the spawned child. There is no other edge-cli
//! freeze subprocess on this fixture's tmp dir; the snapshot path
//! is also unique per run.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;

const EDGE_CLI: &str = env!("CARGO_BIN_EXE_edge-cli");
const WAT_FOREVER: &str = "tests/guests/serve_forever.wat";

/// Compile `serve_forever.wat` to a fresh tmp wasm path. We use the
/// bare WAT (no port rewrite) — the fixture parks in `epoll_wait`
/// regardless of whether a listener is materialized; the freeze
/// quiesce path takes the snapshot whether or not `gs.listener`
/// fires, with the same `try_to_snapshot` code either way.
fn build_parking_wat(path: &std::path::Path) {
    let src =
        std::fs::read_to_string(WAT_FOREVER).unwrap_or_else(|e| panic!("read {WAT_FOREVER}: {e}"));
    // The WAT sets up `sockaddr_in` at memory 4096 with port=0; that's
    // fine — the listener fd gets materialized during _start, the
    // parking loop is reachable, and SIGUSR1 wakes the freeze driver
    // out of the select! before the 10s outer timeout fires.
    let wasm = wat::parse_str(&src).unwrap_or_else(|e| panic!("compile WAT: {e:?}"));
    std::fs::write(path, &wasm).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
}

/// Spawn `edge-cli freeze <wasm> --out <snap>` and return the
/// `Child` handle plus the time the spawn was issued. The caller
/// signals SIGUSR1 after the desired delay and then waits for the
/// child to exit.
fn spawn_freeze(wasm: &Path, snap: &Path) -> tokio::process::Child {
    Command::new(EDGE_CLI)
        .args([
            "freeze",
            wasm.to_str().unwrap(),
            "--out",
            snap.to_str().unwrap(),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn edge-cli freeze")
}

/// ADR 0007 §6: a SIGUSR1 to the freeze process wakes the
/// `call_start` future out of the 3-arm select! before the 10s
/// outer timeout. We assert two things:
///
/// - The child exits well before 10s (we cap at 4s; in practice it
///   fires within ~tens of ms of the SIGUSR1).
/// - The snapshot file is a valid postcard with the right
///   `format_version`.
#[tokio::test(flavor = "current_thread")]
async fn sigusr1_wakes_freeze_before_outer_timeout() {
    let wasm_path = std::env::temp_dir().join("edge_sigusr1_quiesce.wasm");
    let snap_path = std::env::temp_dir().join("edge_sigusr1_quiesce.snap");
    let _ = std::fs::remove_file(&snap_path);
    build_parking_wat(&wasm_path);

    let mut child = spawn_freeze(&wasm_path, &snap_path);
    // Capture the PID before any awaits so we can signal it.
    let pid = child.id().expect("child must have a PID while running") as i32;

    // Wait for the guest to reach the parked epoll_wait. ~500 ms is
    // enough to get past instantiate + socket + bind + listen +
    // epoll_ctl ADD + first epoll_wait. We don't want to send
    // SIGUSR1 too early — the quiesce path is an arm of the select!
    // around `call_start`, so if we wake before the guest parks the
    // freeze will just take the post-`_start` snapshot via the same
    // code path and we still prove the wiring.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // SAFETY: pid is a live child PID we own.
    let rc = unsafe { libc::kill(pid, libc::SIGUSR1) };
    assert_eq!(
        rc,
        0,
        "kill(SIGUSR1) failed: {}",
        std::io::Error::last_os_error()
    );

    // The freeze driver should wake and take the snapshot quickly.
    // 4s is well below the 10s outer timeout, so a quick exit proves
    // SIGUSR1 drove the wake (and not the timeout).
    let status = tokio::time::timeout(Duration::from_secs(4), child.wait())
        .await
        .expect("freeze did not exit within 4s of SIGUSR1 — outer timeout likely fired")
        .expect("wait child");

    assert!(
        status.success(),
        "freeze exited non-zero after SIGUSR1: {status:?}"
    );

    let bytes = std::fs::read(&snap_path).expect("read snapshot file");
    let snap = edge_libos::decode_snapshot(&bytes).expect("decode snapshot");
    assert_eq!(
        snap.format_version.0,
        edge_libos::SNAPSHOT_FORMAT_VERSION,
        "format_version drift"
    );
}

/// Belt-and-braces: without any signal, the freeze driver falls
/// through to the 10s outer timeout. We don't actually wait 10s —
/// we just confirm that with no SIGUSR1 the process is still alive
/// after a short window. This is the negative control for the
/// positive test above: if the positive test "wins" only because
/// freeze always exits fast, this test would also exit fast, which
/// would invalidate the proof.
///
/// We time-bound the assertion: after 1s, the child must still be
/// running. If the freeze driver regresses and exits the moment
/// SIGUSR1 wiring runs (e.g. by mis-handling the listener
/// internally), this test catches it.
///
/// We then kill the child to clean up.
#[tokio::test(flavor = "current_thread")]
async fn freeze_does_not_exit_without_sigusr1() {
    let wasm_path = std::env::temp_dir().join("edge_sigusr1_negative.wasm");
    let snap_path = std::env::temp_dir().join("edge_sigusr1_negative.snap");
    let _ = std::fs::remove_file(&snap_path);
    build_parking_wat(&wasm_path);

    let mut child = spawn_freeze(&wasm_path, &snap_path);
    tokio::time::sleep(Duration::from_millis(500)).await;

    // After 500ms with no signal, freeze must still be parked. The
    // guest is in epoll_wait(timeout=10s); the host driver is in
    // its 3-arm select! awaiting one of: call_start returns,
    // quiesce_notify fires (no SIGUSR1 yet), or 10s timeout. None of
    // those fire by 500ms.
    match child.try_wait().expect("try_wait") {
        None => {}
        Some(status) => panic!(
            "freeze exited unexpectedly without SIGUSR1 (status {status:?}); \
             the quiesce wiring is broken"
        ),
    }

    // Clean up so we don't wait the full 10s timeout per run.
    let _ = child.kill().await;
}
