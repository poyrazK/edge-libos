# edge-libos

A Linux-personality libOS kernel in Rust that runs a real CPython interpreter
(plus the user's FastAPI app) compiled to `wasm32-musl`, inside a Wasmtime
sandbox, through a single async host function `(import "kernel" "syscall")`.

The full design spec lives in [`impelementationplan`](./impelementationplan).
This README is a build-and-run quick reference.

## Status: P0 complete (kernel layer)

All 30 implementation-plan steps are landed:

| # | Step | Artifact |
|---|------|----------|
| 1-14 | Kernel + VFS | `src/{kernel,dispatch,sys,fd,vfs,mm}.rs` |
| 15 | pipe2 | `src/sys/file.rs::pipe2` + vfs_conformance test |
| 16 | C conformance | `tests/conformance/{syscall.h,runner.sh,28 *.c}` |
| 17 | EFAULT fuzzer | `tests/efault_fuzz.rs` (14 tests) |
| 18-19 | CPython guest | `guest/{syscall_shim,build.sh}` |
| 20 | DoD #1 driver | `src/bin/edge_python.rs` + smoke tests |
| 21 | DoD #2 examples | `examples/{print_2_plus_2,import_fastapi}.py` |
| 22 | trace-host | `src/bin/trace_host.rs` (JSON-lines) |
| 23 | strace baselines | `tests/strace_baselines/{strace_native.sh,diff.py,baseline.*.txt}` |
| 24 | reproduce | `scripts/{dev_setup,reproduce_dod}.sh` |
| 25 | open | `src/sys/file.rs::open` (shim over openat) |
| 26 | getcwd | `src/sys/file.rs::getcwd` (write cwd into guest buffer) |
| 27 | stat + lstat | `src/sys/file.rs::{stat,lstat}` (shim over newfstatat) |
| 28 | pipe | `src/sys/file.rs::pipe` (shim over pipe2) |
| 29 | readv + writev | `src/sys/file.rs::{readv,writev}` (scatter/gather over read/write) |
| 30 | wire-up | strace baselines, golden range, README |

The kernel itself handles 40 syscalls; the cross-compiled CPython guest is
the highest-risk artifact and requires `zig cc` + `git submodule` (see
`guest/build.sh`).

## P0 DoD

1. `python -c "print(2+2)"` returns `4` from inside the guest.
2. `import fastapi` succeeds from inside the guest.

The DoD scripts live at `examples/print_2_plus_2.py` and
`examples/import_fastapi.py`. The latter has a stdlib fallback (per
user-confirmed decision #6) when fastapi's extension modules don't
cross-compile cleanly.

## Toolchain

- Rust ≥ 1.85 (pinned via `rust-toolchain.toml`)
- `wasmtime = "=45.0.3"`, `tokio = "1.45"`
- `zig` 0.16.0 (for CPython cross-compile, see `scripts/dev_setup.sh`)
- `wat2wasm` (wabt) for conformance .wat files
- macOS or Linux host
- Optional: `strace` (Linux) or `dtruss` (macOS) for native baselines

## Build

```bash
# Host kernel
cargo build --release

# CPython guest (produces target/wasm32-unknown-linux-musl/release/python.wasm)
bash guest/build.sh
```

## Run

```bash
# P0 DoD #1
cargo run --release --bin edge-python -- \
    target/wasm32-unknown-linux-musl/release/python.wasm \
    examples/print_2_plus_2.py

# P0 DoD #2
cargo run --release --bin edge-python -- \
    target/wasm32-unknown-linux-musl/release/python.wasm \
    examples/import_fastapi.py
```

## Verify (full P0)

```bash
bash scripts/reproduce_dod.sh
```

Runs in order: dev_setup, build, full test suite, guest build (skipped if
no submodule), DoD #1 + #2 (driver-level smoke tests if no guest), and
syscall-trace diff vs native baselines.

## Test layout

- `tests/*_conformance.rs` — Rust-side per-syscall unit tests
- `tests/conformance/` — C-side integration tests through musl libc
- `tests/efault_fuzz.rs` — pointer-poison fuzzer across all syscalls
- `tests/edge_python_smoke.rs` — DoD #1 driver smoke (stdout + exit code)
- `tests/edge_python_import_smoke.rs` — DoD #2 driver smoke (realistic import mix)
- `tests/trace_host_smoke.rs` — JSON-output contract
- `tests/strace_baseline_diff.rs` — diff harness (auto-skips raw strace)

## Repo layout

See the [P0 implementation plan](.claude/plans/cryptic-munching-crystal.md) for
the file-by-file map. The host kernel is in `src/`; the CPython guest build is
in `guest/`; verification artifacts live in `tests/`.