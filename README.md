# edge-libos

[![CI](https://img.shields.io/badge/verify-pending-lightgrey)](./.github/workflows/ci.yml)
<!--
  Replace the badge URL with the live GitHub Actions badge once the
  repo is published to GitHub:
    https://github.com/<owner>/edge-libos/actions/workflows/ci.yml/badge.svg
  Until then, this static badge is a placeholder.
-->

A Linux-personality libOS kernel in Rust that runs a real CPython interpreter
(plus the user's FastAPI app) compiled to `wasm32-musl`, inside a Wasmtime
sandbox, through a single async host function `(import "kernel" "syscall")`.

The full design spec lives in [`impelementationplan`](./impelementationplan).
This README is a build-and-run quick reference.

## Status: P1 complete, P2 in progress

**P0** — boots CPython, prints: ✅
**P1** — serves one HTTP request via the WAT uvicorn+FastAPI syscall sequence
through the full async pivot (epoll/eventfd): ✅ (`ec911cb`)
**P2** — production-ish single instance: 🚧 in progress

P2 adds pre-init snapshot/restore (sub-5 ms cold start), host-backed DNS
resolver, default-deny egress policy, epoch-based CPU-ms metering, and
minimal AF_UNIX support, alongside the literal CPython cross-compile
pipeline. See [`HANDOFF.md`](./HANDOFF.md) for the running status; see
the P2 plan for the full breakdown.

## P1 DoD (satisfied)

P1 closed with the 8-step uvicorn+FastAPI syscall-surface coverage:

| # | Step | Artifact |
|---|------|----------|
| 1 | socket(2) | `src/sys/socket.rs::socket` |
| 2 | bind(2) + listen(2) | `src/sys/socket.rs::{bind,listen}` |
| 3 | setsockopt + O_NONBLOCK | `src/sys/socket.rs::setsockopt` |
| 4 | accept4(2) async | `src/sys/socket.rs::accept4` |
| 5 | connect + sendto + recvfrom | `src/sys/socket.rs::{connect,sendto,recvfrom}` |
| 6 | getsockopt / getsockname / getpeername / shutdown / poll | `src/sys/socket.rs::*`, `src/sys/poll.rs` |
| 7 | epoll_create1 + epoll_ctl + epoll_wait + eventfd2 | `src/sys/{epoll,eventfd}.rs` |
| 8 | serve one HTTP request | `tests/guests/serve_one_request.wat` + smoke tests |

The kernel handles **58 NRs** across 11 modules (`process`, `memory`,
`file`, `socket`, `poll`, `epoll`, `eventfd`, `identity`, `time`,
`random`, `signal`). The cross-compiled CPython guest is the highest-risk
artifact and requires `zig cc` + a git submodule — see `guest/build.sh`.

## Test totals

- **39** Rust unit tests (in `#[cfg(test)]` modules under `src/`)
- **158** Rust integration tests (across `tests/*.rs`)
- **44** C conformance tests (`tests/conformance/*.c`)
- **Total: 241 tests, all green.**

Source of truth: `bash tests/count_tests.sh`. The conformance runner
also prints the total at the end of its run.

## P0 DoD

1. `python -c "print(2+2)"` returns `4` from inside the guest.
2. `import fastapi` succeeds from inside the guest.

The DoD scripts live at `examples/print_2_plus_2.py` and
`examples/import_fastapi.py`. The latter has a stdlib fallback (per
user-confirmed decision #6) when fastapi's extension modules don't
cross-compile cleanly.

## P2 DoD (in progress)

Per spec §7:

> Existing FastAPI+pydantic+httpx+SQLAlchemy-over-Postgres app deploys
> unchanged, cold-starts < 5 ms, meters CPU-ms.

Concretely P2 lands (in order):

1. **Hygiene** (A1-A7) — marker-enforced C conformance runner, dispatch
   dedup, reproducible DoD script, README. ✅ done.
2. **Syscall gaps** (B1-B6, C1-C3) — eventfd generic R/W, getdents64
   stream position, getsockopt -EBADF, async poll, statx, dup/dup2/dup3,
   file-op batch, identity/process/signal batch, sockets/poll/eventfd
   completion with **AF_UNIX** support.
3. **Snapshot/restore** (D1-D3) — `postcard` + serde snapshot of kernel
   state + linear memory; `edge-python freeze`/`serve` subcommands;
   50-iteration cold-start benchmark under 5 ms p50.
4. **DNS + egress** (E1-E2) — default-deny egress policy, host-backed
   resolver via `hickory-resolver`, new `NR_RESOLVE` syscall.
5. **Metering** (F1-F2) — epoch-based interruption, 100 ms default
   budget per request, `cpu_ms_used` accounting.
6. **Literal CPython DoD** (A6) — `guest/cpython` submodule cross-compile
   via `zig cc`, real `edge-python serve_one_request.py` produces
   `200 OK` from a real CPython+uvicorn+FastAPI wasm module.
7. **CI** (G3 / CI-1) — `.github/workflows/ci.yml` running all 8 steps of
   `reproduce_dod.sh` on Linux. ✅ CI-1 landed; a local mirror lives
   at `scripts/preflight.sh`, and branch-protection instructions
   are at `docs/branch-protection.md`.

See [`HANDOFF.md`](./HANDOFF.md) for the full running status.

## Toolchain

- **Rust 1.93.0** (pinned via `rust-toolchain.toml`)
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

## Verify

```bash
# Canonical test totals
bash tests/count_tests.sh

# Full Rust suite
cargo test --release

# Marker-enforced C conformance
bash tests/conformance/runner.sh

# Full DoD sequence (8 steps)
bash scripts/reproduce_dod.sh
```

`reproduce_dod.sh` runs in order: dev_setup, build, full test suite,
C conformance, strace-baseline diff, guest build (skipped if no
submodule), DoD #1 + DoD #2 (driver-level smoke tests if no guest),
DoD #3 (real uvicorn+FastAPI serve), and the canonical test totals.

## Test layout

- `src/**/tests.rs` / `#[cfg(test)] mod tests` — Rust unit tests
- `tests/*_conformance.rs` — Rust-side per-syscall integration tests
- `tests/conformance/` — C-side integration tests through musl libc
  (one .c per syscall, marker-enforced via `mark_pass`/`mark_fail`)
- `tests/efault_fuzz.rs` — pointer-poison fuzzer across all syscalls
- `tests/edge_python_smoke.rs` — DoD #1 driver smoke (stdout + exit code)
- `tests/edge_python_import_smoke.rs` — DoD #2 driver smoke (realistic import mix)
- `tests/trace_host_smoke.rs` — JSON-output contract
- `tests/strace_baseline_diff.rs` — diff harness (auto-skips raw strace)
- `tests/count_tests.sh` — single source of truth for the test total

## Repo layout

- `src/` — host kernel (one Cargo package, no workspace)
  - `kernel.rs`, `dispatch.rs` — Kernel state + `kernel.syscall` dispatcher
  - `sys/*.rs` — per-syscall handlers (process, memory, file, socket, …)
  - `vfs.rs`, `fd.rs`, `mm/` — VFS, fd table, memory arena
  - `bin/` — `edge-python` driver, `trace-host` tracer
- `tests/` — Rust integration tests + C conformance suite
- `tests/conformance/` — C conformance (.c files + zig-built .wasm + runner.sh)
- `guest/` — CPython cross-compile pipeline + syscall shim
- `examples/` — DoD scripts (`print_2_plus_2.py`, `import_fastapi.py`,
  `serve_one_request.py`)
- `scripts/` — `dev_setup.sh`, `reproduce_dod.sh`

See [`impelementationplan`](./impelementationplan) for the full syscall-by-syscall spec.