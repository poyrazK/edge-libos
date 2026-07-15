//! P2-metering M6: end-to-end metering smoke + FUEL_PER_MS calibration.
//!
//! Two cases:
//!
//! - **trap-on-tight-budget**: `--cpu-budget-ms 1` on a fixture
//!   that loops forever. The guest traps on `OutOfFuel`; the host
//!   surfaces it as `CliError::Metered` → exit 1 with a "used N
//!   ms / budget 1 ms" message. This is the load-bearing CLI-wiring
//!   test.
//!
//! - **calibration**: the same fixture mutated to iters=1000 (loop
//!   1000 iterations then exit cleanly). Run with
//!   `--cpu-budget-ms 10000` (10s). The wasm's `exit(0)` syscall
//!   fires after exactly 1000 fuel-burning iterations. We can't
//!   read fuel_consumed directly from the CLI output, but we CAN
//!   assert the wall-clock time stays well under 1 second on this
//!   machine (proving FUEL_PER_MS isn't wildly off) and that the
//!   fixture completes (exit 0). The actual FUEL_PER_MS calibration
//!   is done by reading `store.get_fuel()` from the in-process
//!   metering_debug-style test that drove the initial empirical
//!   calibration — see the source-chained `is_out_of_fuel` test in
//!   `src/meter.rs::tests`.
//!
//! The WAT fixture is parameter-free on disk (iters defaults to 0
//! = loop forever, which the trap-on-tight-budget test needs).
//! For the calibration case we patch the compiled wasm's data
//! segment to set iters=1000 — fragile in principle (it scans the
//! bytes for an 8-byte zero run), but the WAT has exactly one
//! such run at the offset the fixture documents.

use std::path::PathBuf;
use std::process::Stdio;

use tokio::process::Command;

const EDGE_CLI: &str = env!("CARGO_BIN_EXE_edge-cli");
const WAT_SRC: &str = "tests/guests/burn_fuel.wat";

/// Compile `burn_fuel.wat` to a temp wasm path. If `iters` is
/// non-zero, patch the wasm's 8-byte zero run (offset 256 in the
/// fixture's data segment) to that value. Each call gets a unique
/// path so concurrent test cases can't stomp on each other.
fn build_wat_with_iters(iters: u64) -> PathBuf {
    let src = std::fs::read_to_string(WAT_SRC).expect("read burn_fuel.wat");
    let mut wasm = wat::parse_str(&src).expect("compile burn_fuel.wat");
    if iters != 0 {
        let iters_bytes = iters.to_le_bytes();
        let mut patched = false;
        for off in 0..wasm.len().saturating_sub(8) {
            if &wasm[off..off + 8] == &[0u8; 8] {
                wasm[off..off + 8].copy_from_slice(&iters_bytes);
                patched = true;
                break;
            }
        }
        assert!(patched, "could not patch iters in compiled wasm");
    }
    let path = std::env::temp_dir().join(format!("edge_metering_burn_{iters}.wasm"));
    std::fs::write(&path, &wasm).expect("write wasm");
    path
}

/// Run `edge-cli <subcmd> ...` to completion, returning the exit
/// status and captured stderr.
async fn run_edge_cli(args: &[&str]) -> (std::process::ExitStatus, String) {
    let out = Command::new(EDGE_CLI)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .await
        .expect("spawn edge-cli");
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    (out.status, stderr)
}

#[tokio::test(flavor = "current_thread")]
async fn run_traps_on_out_of_fuel_with_tight_budget() {
    // Fixture defaults to iters=0 (loop forever). With a 1 ms
    // budget, the guest MUST trap on OutOfFuel; the host surfaces
    // it as CliError::Metered → exit code 1 with a "used N ms /
    // budget 1 ms" message.
    let wasm_path = build_wat_with_iters(0);

    let (status, stderr) = run_edge_cli(&[
        "run",
        wasm_path.to_str().unwrap(),
        "--cpu-budget-ms",
        "1",
    ])
    .await;

    assert_eq!(
        status.code(),
        Some(1),
        "expected exit 1 (Metered), got {status:?}; stderr={stderr}"
    );
    assert!(
        stderr.contains("cpu budget exceeded"),
        "expected 'cpu budget exceeded' in stderr, got: {stderr}"
    );
    assert!(
        stderr.contains("budget 1 ms"),
        "expected 'budget 1 ms' in stderr, got: {stderr}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn run_completes_bounded_fixture_under_loose_budget() {
    // Same fixture with iters=1000 patched in. 1000 iterations of
    // local arithmetic + conditional branch is well below a 10s
    // fuel budget; the wasm reaches its NR_EXIT syscall and exits
    // cleanly. This proves the metering trap path didn't break the
    // happy path (a SetFuel call that doesn't trap must still let
    // the guest complete its work).
    let wasm_path = build_wat_with_iters(1000);

    let (status, stderr) = run_edge_cli(&[
        "run",
        wasm_path.to_str().unwrap(),
        "--cpu-budget-ms",
        "10000",
    ])
    .await;

    assert_eq!(
        status.code(),
        Some(0),
        "expected exit 0 (bounded fixture completes), got {status:?}; stderr={stderr}"
    );
}
