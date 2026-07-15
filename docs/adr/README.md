# Architecture Decision Records

This directory holds the ADRs for `edge-libos`. Each ADR pins a
concrete decision before the implementer lands the code, so the
implementation has a contract to honor.

## Index

- [ADR 0001 — P3 futex semantics](0001-p3-futex-semantics.md) — Accepted.
  Pinned the `futex(2)` integration contract (u32 guest addresses,
  per-PID `Notify` scheme, snapshot allowlist format).
- [ADR 0002 — snapshot wire format](0002-snapshot-wire-format.md) — Accepted.
  Pinned the `postcard` container, explicit `LeU32` / `LeU64` newtypes,
  sparse per-page memory layout, format-version rule.
- [ADR 0003 — P3 live migration](0003-p3-live-migration.md) — Accepted.
  Pinned the v1 freeze-then-serve migration flow: module portability,
  drain semantics, format-version interaction, accepted-stream +
  abstract-namespace rejection, and the `Subcommand::Migrate` wrapper.
- [ADR 0004 — freeze / serve wire contract (P2-D3.5)](0004-freeze-serve-wire.md) — Proposed.
  Pins the v1 contract for `NR_SNAPSHOT = 123` (guest-driven quiescence),
  `EDGE_SERVE_FD_<N>` env-var fd-inherit shape, and the subprocess flow
  that `edge-cli migrate` orchestrates via `Command::new(current_exe)`.
- [ADR 0005 — snapshot module-hash portability check (P3-D3.5-followup-1)](0005-snapshot-module-hash.md) — Accepted.
  Pins SHA-256 of the freeze-side `.wasm` bytes inside
  `KernelSnapshot.module_sha256`; `verify_module_hash` refuses to
  apply on mismatch at the `serve` boundary. Closes the silent
  mis-execution caveat formerly called out by ADR 0002 §8.