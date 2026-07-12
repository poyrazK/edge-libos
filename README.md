# edge-libos

A Linux-personality libOS kernel in Rust that runs a real CPython interpreter
(plus the user's FastAPI app) compiled to `wasm32-musl`, inside a Wasmtime
sandbox, through a single async host function `(import "kernel" "syscall")`.

The full design spec lives in [`impelementationplan`](./impelementationplan).
This README is a build-and-run quick reference.

## Status: P0 (CPython boots and prints)

P0 DoD:
1. `python -c "print(2+2)"` returns `4` from inside the guest.
2. `import fastapi` succeeds from inside the guest.

## Toolchain

- Rust ≥ 1.85 (pinned via `rust-toolchain.toml`)
- `wasmtime = "=45.0.3"`, `tokio = "1.45"`
- `zig` ≥ 0.13 (for CPython cross-compile, see `scripts/dev_setup.sh`)
- macOS or Linux host

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

Runs in order: unit tests, conformance suite, EFAULT fuzzer, guest build, both
DoD scripts, syscall-trace diff vs native strace goldens.

## Repo layout

See the [P0 implementation plan](.claude/plans/cryptic-munching-crystal.md) for
the file-by-file map. The host kernel is in `src/`; the CPython guest build is
in `guest/`; verification artifacts live in `tests/`.
