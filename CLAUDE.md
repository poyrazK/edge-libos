# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

`edge-libos` is a general-purpose Linux-personality libOS **kernel written in
Rust**. It runs **any `wasm32-musl` guest** that speaks the Linux x86-64 syscall
ABI, inside a Wasmtime sandbox, through a **single async host function**:
`(import "kernel" "syscall")`. The kernel implements a growing subset of Linux
syscalls (currently ~90 NRs across process/memory/file/socket/poll/epoll/
eventfd/identity/ioctl/time/random/signal) on top of Wasmtime — it is not tied
to any particular guest.

CPython + FastAPI is the **reference workload** the milestones validate against
(it's the highest-risk guest and exercises the widest syscall surface), but the
kernel, the syscall handlers, and the conformance suite are guest-agnostic: the C
conformance tests are plain `wasm32-musl` C programs, and `edge-cli` (any
subcommand) loads any conforming module. Treat CPython as the driving example,
not the scope.

The full design spec is [`impelementationplan`](impelementationplan) (source of
truth for decisions). [`README.md`](README.md) is the build/run quick reference.
[`HANDOFF.md`](HANDOFF.md) is a regenerated, uncommitted running status.

Milestones use CPython as the acceptance criterion: P0 (boot a guest — proven by
CPython) ✅, P1 (serve one HTTP request via uvicorn's epoll/eventfd syscall
sequence) ✅, P2 (production-ish single instance) 🚧.

##Behavioral 

Never commit main unless user wants 
Our workflows : determine the goal/issue , create imp plan , create new branch , commit small , create pr , review , verify all ci green and merge 
NEVER EVER merge , its users job 

## Commands

**Iterating locally? Do NOT use `--release`, and do NOT run the full suite each
loop.** `--release` uses `panic="abort"`, so `build` then `test` recompiles the
whole Wasmtime dep graph *twice*; agents work in separate worktrees, so every
cold `--release` build is a fresh full Wasmtime compile. Instead:

```bash
cargo build --profile ci                          # shares ONE dep compile with test
cargo nextest run --profile ci <substring>        # fast, parallel; scope to what changed
cargo test  --profile ci <substring>              # if a test isn't nextest-compatible
```

Reserve `--release` and the **full** suite for pre-merge / CI only — CI already
runs everything on every push. The commands below use `--release` for
completeness; substitute `--profile ci` when iterating.

```bash
# Build host kernel (single binary: edge-cli with subcommands)
cargo build --release

# Full Rust suite (unit + integration)
cargo test --release

# One test binary / one test by name
cargo test --release --test <file_stem>        # e.g. --test socket_conformance
cargo test --release <substring>               # filters by test name

# C conformance suite (needs zig; builds edge-cli if missing)
bash tests/conformance/runner.sh

# Canonical test total (single source of truth; runner.sh must agree)
bash tests/count_tests.sh

# Full 8-step DoD sequence
bash scripts/reproduce_dod.sh
# Fast local CI mirror
bash scripts/preflight.sh

# CPython guest cross-compile (highest-risk; needs zig 0.16 + cpython submodule)
bash guest/build.sh

# Run a guest wasm
cargo run --release --bin edge-cli -- run <python.wasm> [--] [args...]
```

Toolchain is pinned: **Rust 1.93.0** (`rust-toolchain.toml`), `wasmtime =45.0.3`,
`tokio 1.45`, `zig 0.16.0` (guest + C conformance), `wat2wasm`/wabt. Use
`cargo fmt` / `cargo clippy` (both components are installed via the toolchain).

**Build-profile gotcha:** `[profile.release]` uses `panic = "abort"`, which
forces `cargo test` to recompile the whole dep graph under `panic=unwind`. CI
uses `--profile ci` (opt-level 1, lto off, `panic=unwind`) so `build` and `test`
share one compile. `reproduce_dod.sh` honors `SKIP_DOD_SMOKE=1` /
`SKIP_TEST_TOTALS=1`; `runner.sh` honors `SKIP_TEST_COUNT=1` — these avoid
hidden full wasmtime recompiles in CI.

## Architecture

**The entire host ABI is one function.** `dispatch::register` installs
`kernel.syscall` on the Wasmtime `Linker` as an async host func taking **7 i64
params** (`nr` + 6 args) and returning **1 i64**. `dispatch::dispatch` matches
`nr` onto a handler under `crate::sys::*`; the default arm returns `-ENOSYS`
(clean, never a crash — a missing syscall becomes a visible build/import error,
per spec §9).

**Return convention (kernel-style):** handlers return `i64` where `>= 0` is
success and `[-4095, -1]` is `-errno`. Errno constants live in
[`src/errno.rs`](src/errno.rs); use `to_ret(POSITIVE_ERRNO)` to negate. The
guest's musl translates the negative return back into `errno`.

**Per-store state is `Kernel`** ([`src/kernel.rs`](src/kernel.rs)): linear
`memory` (attached post-instantiation), `fds` (fd table), `vfs`, `mm` (linear
allocator / brk), clock, args/env, rng, signal state, `exit_code`. Handlers
reach it via `Caller::data()` / `Caller::data_mut()`.

**All guest-pointer access goes through [`src/mem.rs`](src/mem.rs)** —
`guest_slice`, `guest_slice_mut`, `guest_str`. This is the **EFAULT choke
point**: every ptr+len is bounds-checked against linear memory; a bad guest
pointer returns `-EFAULT`, never a host segfault (a host segfault would be a
sandbox escape, spec §8). Never index guest memory directly.

**fd model** ([`src/fd.rs`](src/fd.rs)): `FdTable` maps fds to a `Resource` enum
(Stdin/Stdout/Stderr pipes, File, PipeRead/Write, Socket, Epoll, EventFd).
dup-able resources (File, Socket) hold `Arc<Mutex<..>>` shared state
(`SharedFilePos`, `SharedSocket`) so `dup`/`dup2` share the open-file
description. These use **`parking_lot::Mutex` (sync) — never hold a lock across
`.await`**.

**Async pivot:** handlers are `async fn` even when synchronous (sync ones just
return immediately). The socket/epoll/poll path is genuinely async on a
tokio current-thread runtime, one `Store` per fiber (see Multi-fiber below).
epoll_wait uses `tokio::select!` over a timeout + per-fd `Notify` + cancel.

**Multi-fiber (P3 Tier-3, ADR 0001 §2):** `wasm_threads(true)` +
`shared_memory(true)` + `wasm_shared_everything_threads(true)` are all
enabled in `src/host.rs::build_engine` (and `Cargo.toml:22` adds `"threads"`
to the wasmtime feature list). `Store` is still `!Send`/`!Sync` — each
fiber pins to its host thread — so cross-fiber wakeups go through
shared-memory atomics (`memory.atomic.notify` / `memory.atomic.wait32`) on
a `wasmtime::SharedMemory`, not by moving a `Store` between threads. The
kernel's `Arc<Notify>` machinery is now reachable from guest fibers hosted
in different `Store`s. Future `clone(56)` / `fork(57)` handlers must
respect this constraint (spawn a new tokio task + new thread-local `Store`
rather than moving the existing one).

### One host binary, many subcommands (guest-agnostic)

`edge-cli` ([`src/bin/edge_cli.rs`](src/bin/edge_cli.rs)) is the single host
binary; `src/cli/mod.rs::run_main` dispatches by subcommand:

- `run <wasm> [--] [args...]` — instantiates the guest, attaches linear memory
  after instantiation, calls `_start`, drains buffered stdout/stderr,
  propagates the guest exit code.
- `trace <wasm> [--diff <baseline>] [--no-marker]` — installs a
  `SyscallObserver` (via `install_observer`) and emits one JSON line per
  syscall. It does **not** re-mirror the dispatch table, so new syscalls are
  picked up automatically. Supports `--diff <baseline>` (fail if a baseline
  syscall is missing).
- `freeze <wasm> [--] [args...] --out <path>` — instantiates the
  guest, drives it with a bounded 10s timeout (covers
  short-lived guests + server-style guests parked in
  `accept4`/`epoll_wait`), then writes a postcard snapshot.
  Includes the **ephemeral-port-drift fix** at
  `src/snapshot.rs:560-580` — without it, snapshots taken
  from `bind(0.0.0.0:0)` would record `bound.port=0` and
  `apply_snapshot` would bind a *different* port than the
  snapshot says.
- `serve <snap> <wasm> [--port <p>]` — reads a snapshot,
  instantiates the matching wasm, applies kernel state +
  memory, then respawns the guest at the post-snapshot
  state. `--port <p>` pre-mutates the snapshot's V4 listener
  bound port so `apply_snapshot` rebinds to `<p>` rather than
  the WAT-recorded port.
- `bench <snap> <wasm> --iters <n>` — hand-rolled
  p50/p95/p99/max over `apply_snapshot_*` cycles (engine
  construct hoisted out of the iter loop; ~10 ms saved/iter).
  Zero new deps (no criterion, no divan). `p50 < 5 ms` gate;
  violation → `CliError::Bench → exit 1`.

The binary embeds nothing CPython-specific — the runtime accepts any guest
whose imports are satisfied by `kernel.syscall` and imported memory/table.

### The reference guest (CPython)

CPython is one guest, cross-compiled by [`guest/build.sh`](guest/build.sh) using
`zig cc -target wasm32-freestanding`, against musl headers (no libc — we provide
our own syscall shim in `guest/syscall_shim/`, which is reusable by any C guest),
linked with `--import-memory --import-table`. The `guest/cpython` submodule is
**not** checked in / not in the workspace; guest-dependent steps auto-skip when
it's absent. The C conformance tests under `tests/conformance/` are the smallest
example guests and are the best reference for what a conforming module looks
like.

## Adding or changing a syscall

1. Define `pub const NR_* ` and an `async fn` handler in the right
   `src/sys/<group>.rs` (groups: process, memory, file, socket, poll, epoll,
   eventfd, identity, ioctl, time, random, signal, path).
2. Add a match arm in **`dispatch::dispatch`** ([`src/dispatch.rs`](src/dispatch.rs)).
3. Add a match arm in **`dispatch::syscall_name`** (same file) — the conformance
   runner and `edge-cli trace` need the name, and the runner fails loudly for an
   unregistered name.
4. Add tests: a Rust integration test in `tests/*_conformance.rs`, and usually a
   C test in `tests/conformance/<name>.c`. Update `expected_syscall()` in
   `tests/conformance/runner.sh` to map the test → expected observed syscall.

### wasm32-musl ABI gotchas (these silently corrupt data)

- **`long` is 32 bits on wasm32-musl.** Shifting an `int` by ≥32 is UB, and
  `zig cc -O2` silently corrupts locals. Decode 8-byte fields with `int64_t`.
- **`socket(2)` type is `type & 0xf`** — `SOCK_NONBLOCK` (0o4000) and
  `SOCK_CLOEXEC` (0o2000000) are *separate* high bits, not part of the type.
- When a test needs input in guest memory, **write it only after
  `attach_memory`** — before that, `Kernel::memory()` returns `-EFAULT`.

## Test layout

- Rust unit tests — `#[cfg(test)] mod tests` inside `src/**`.
- Rust integration — `tests/*.rs` (per-syscall `*_conformance.rs`, plus
  `efault_fuzz.rs` pointer poisoner, `*_smoke.rs` driver smokes,
  `strace_baseline_diff.rs`).
- **C conformance** — `tests/conformance/*.c`, one file per syscall, compiled
  with `zig cc`, driven through `edge-cli trace`, and verified by observing the
  expected syscall in the JSON trace. Each test is **marker-enforced**: it calls
  `mark_pass()` / `mark_fail(reason)` from `tests/conformance/syscall.h`.
- `tests/count_tests.sh` is the one source of truth for the total; `runner.sh`
  prints the same number.
