# ADR 0005 — snapshot module-hash portability check (P3-D3.5-followup-1)

- **Status.** Accepted, 2026-07-15. Realized by P3-D3.5-followup-1
  on branch `p3-d35-followup-snapshot-portability`: SHA-256 of the
  freeze-side `.wasm` bytes embedded in `KernelSnapshot`, verified
  on serve before any apply step.
- **Phase.** P3 follow-up after D3.5 (`impelementationplan` §7).
  Closes the silent-mis-execution hazard called out by ADR 0002 §8.
- **Scope.** The wire-level guarantee that `edge-cli serve` will
  refuse to apply a snapshot whose frozen-hash does not match the
  `.wasm` file the operator is now handing us. Closes the
  silent-mis-execution hazard for both prod-shape and bench flows.

## Context

`edge-cli serve` accepts two paths: a `.snap` and a `.wasm`. Today
the serve path trusts that the `.wasm` path matches the one that
was frozen. If an operator (or an automation bug) feeds serve a
different `.wasm` — a stale build, a different version, an
unrelated guest entirely — `apply_snapshot_kernel_state` succeeds,
the linear-memory overlay is written, the guest is respawned, and
the result is **silent mis-execution**: wrong function pointers,
wrong global layout, wrong data segment, no error. The failure is
only observable by reading the guest's output, and only if you
happen to know what "correct" looks like.

ADR 0002 §8 calls this out verbatim as a caveat NOT addressed by
that ADR:

> Caveat (NOT addressed by this ADR): bench trusts the wasm
> path matches freeze's. Future: embed a module hash in
> `KernelSnapshot`, bump `SNAPSHOT_FORMAT_VERSION`, refuse to
> apply if hashes disagree.

P2-D3.5 (PR #24, merge `6ec5428`) landed the freeze/serve bodies
without addressing the caveat — the follow-up was reserved as the
next concrete PR. This ADR is the contract for that follow-up.

## Decision

P3-D3.5-followup-1 MUST satisfy the following properties.

### 1. Hash is the SHA-256 of the raw wasm file bytes

The freeze CLI computes `Sha256::digest(wasm_bytes)` over the
exact bytes the operator handed to `edge-cli freeze`, **before**
those bytes are passed to `wasmtime::Module::new` or
`Module::deserialize`. For raw `.wasm` files this is the bytes
wasmtime parses; for precompiled wasmtime artifacts (the
`Module::deserialize` branch) it is the bytes the serve side will
also deserialize. Either way: same bytes in, same hash out.

The hash is NOT recomputed on serve — it is embedded in the
snapshot at freeze time and read back on serve, compared against
the SHA-256 of the serve-side bytes the operator hands to
`edge-cli serve`.

### 2. Wire-format: append `module_sha256: [u8; 32]` to `KernelSnapshot`

Append `module_sha256: [u8; 32]` as the **last field** of
`KernelSnapshot`, immediately after `cpu_ns`, behind
`#[serde(default)]`. Raw byte array — same shape as the existing
`rng_seed: [u8; 32]` and `comm: [u8; 16]` fields.

`[u8; 32]` is byte-order-portable by construction (single bytes
have no endianness); no `LeU32` sharding needed. `postcard`
serializes a `[u8; 32]` as a fixed 32-byte sequence — no length
prefix because the size is statically known. **Wire-format
impact: +32 bytes per snapshot.**

### 3. `SNAPSHOT_FORMAT_VERSION` stays at 1

This is the load-bearing consequence of the additive precedent
established by ADR 0004 §4:

| Field | Where | Bumped version? |
|---|---|---|
| `SocketSnapshot.inherited: bool` | `src/snapshot.rs:367` | No — stayed at 1 |
| `KernelSnapshot.cpu_ns: LeU64` | `src/snapshot.rs:194` | No — stayed at 1 |
| `KernelSnapshot.futex_table: Vec<…>` | `src/snapshot.rs:186` | No — stayed at 1 |
| `KernelSnapshot.module_sha256: [u8; 32]` | (this ADR) | No — stays at 1 |

`#[serde(default)]` makes the new field opt-in on the deserializer
side: any v1 snapshot written before this ADR decodes cleanly
with `module_sha256 = [0u8; 32]`. No migration path, no
`FormatVersionMismatch` on old snapshots, no parallel "v2" wire
form to maintain.

If a future ADR decides strict hash verification must become
mandatory for ALL snapshots (including pre-existing ones), that
is a separate decision that would invalidate the additive
precedent and require a `FormatVersionMismatch` migration. Out
of scope for this ADR.

### 4. The `[0u8; 32]` skip-verify quirk

When `snap.module_sha256 == [0u8; 32]`, `verify_module_hash`
returns `Ok(())` and emits a `tracing::warn!` so the operator
knows they are running an unverified restore:

> snapshot has no recorded module sha256; skipping portability
> check. Re-freeze with the updated edge-cli to enable strict
> verification.

This quirk is necessary because pre-existing v1 snapshots have
no recorded hash — the additive precedent says we don't break
them, but the verify path needs SOMETHING to do with them.
Skip-with-warn is the additive compromise. Operators who want
strict verification re-freeze with the updated `edge-cli freeze`
(a one-time cost).

### 5. New `SnapshotError::ModuleHashMismatch` variant

```rust
SnapshotError::ModuleHashMismatch {
    snap_hash: [u8; 32],
    wasm_hash: [u8; 32],
},
```

Display arm prints both hashes as lowercase hex so the operator
can `sha256sum` their `.wasm` file and see immediately why the
snapshot was rejected. Mapped to `CliError::Snapshot` → exit 1
via the existing dispatcher (`src/cli/mod.rs`).

### 6. Verify lives at the `serve` boundary, NOT inside `apply_snapshot_kernel_state`

`verify_module_hash(snap, &wasm_bytes)` is called explicitly in
`edge-cli serve::serve_loop` between module instantiation and
`apply_snapshot_kernel_state`. The `apply_snapshot_*` family
keeps its current signature (no `_with_hash` variant).

Rationale:

- Symmetric with the existing three-step apply orchestration
  (`apply_snapshot_inherited_listeners` etc.) — adding a fourth
  explicit `verify` call is the natural shape.
- The verify step needs wasm bytes; only the CLI has them. The
  `NR_SNAPSHOT = 123` guest-driven path can't supply a hash, so
  `try_to_snapshot` / `build_kernel_snapshot` keep building
  with `module_sha256 = [0u8; 32]`. The freeze CLI overwrites
  the field on the returned `KernelSnapshot` before writing to
  disk.
- Tests using the integration path
  (`tests/snapshot_roundtrip.rs`) keep working unchanged because
  they call `try_to_snapshot` / `apply_snapshot_kernel_state`
  directly without going through the CLI verify boundary.

### 7. Hash the bytes, not the parsed module

We hash the FILE BYTES, not the post-parse `wasmtime::Module`.
This is the simplest contract: same bytes on both sides, same
hash. We do NOT hash the parsed module's serialized form because
that would require an extra `Module::serialize` step and would
change the hash if wasmtime's serialized form ever changes
(internal-format hazard). File bytes are stable across wasmtime
versions.

## Consequences

### What this ADR mandates on P3-D3.5-followup-1

- `Cargo.toml` adds `sha2 = "0.10"` to `[dependencies]`.
- `src/snapshot.rs` adds the `module_sha256` field, the
  `ModuleHashMismatch` variant, the `verify_module_hash` free
  function, and the `hex_lower32` private helper for the
  Display arm.
- `src/cli/freeze.rs::freeze_snapshot` computes SHA-256 of the
  wasm bytes once (single `std::fs::read` → both the hasher and
  the `Module::new`/`Module::deserialize` branch) and sets
  `snap.module_sha256` on the returned `KernelSnapshot` before
  `Ok(snap)`.
- `src/cli/serve.rs::serve_loop` calls `verify_module_hash(snap,
  &wasm_bytes)?;` between module instantiation and the
  three-step apply. `?` propagates the
  `SnapshotError::ModuleHashMismatch` variant to
  `CliError::Snapshot` → exit 1.
- Three snapshot-module unit tests
  (`verify_module_hash_rejects_mismatch`,
  `verify_module_hash_accepts_matching`,
  `verify_module_hash_skips_when_unset`).
- Two CLI hash unit tests
  (`freeze_writes_module_sha256_to_snapshot`,
  `serve_rejects_snapshot_with_wrong_wasm_hash`).
- One e2e test (`cli_migration_e2e_rejects_mismatched_wasm`)
  that runs the real subprocess pair with deliberately
  different wasm paths and asserts serve exits 1 with
  "module hash mismatch" on stderr.

### What this ADR enables

- Cross-host migration (P3 live migration, ADR 0003) gets the
  same portability guarantee for free — any `apply_snapshot_*`
  call site that flows through the CLI's `serve_loop` inherits
  the verify step. The in-process `MIGRATE_IN_PROCESS=1` test
  opt-in shares the same `serve_loop` call site and inherits
  the verify.
- Future ADR that wants to mandate strict verification across
  ALL snapshots (closing the skip-verify quirk) can do so by
  bumping `SNAPSHOT_FORMAT_VERSION` to 2 and re-snapshotting.
  This ADR explicitly leaves that decision for later.

### What this ADR blocks

- Freeze-side code that mutates the wasm bytes (e.g. a future
  "strip debug sections" optimization) BEFORE hashing. The
  contract is file bytes in, hash out — the operator's file
  on disk must hash to whatever is on the snapshot. If a future
  optimization needs to run before hashing, it must run on
  BOTH the freeze side and the serve side (or the hash check
  becomes a different check).
- `Module::serialize` as the source of truth for the hash. We
  deliberately hash the raw file bytes (D7 above).

## References

- ADR 0002 §8 — the "NOT addressed" caveat this ADR closes.
- ADR 0004 §4 — the additive-end-of-struct precedent this ADR
  follows (the `SocketSnapshot.inherited` and `cpu_ns` fields
  did not bump `SNAPSHOT_FORMAT_VERSION` either).
- `impelementationplan` §7 (P2 production-ish single instance —
  D3.5 follow-up).
- `HANDOFF.md` §"D3.5 follow-ups" item 1 (the next concrete PR
  before this ADR).
- `src/snapshot.rs` — `module_sha256` field, `verify_module_hash`
  free function, `ModuleHashMismatch` variant.
- `src/cli/freeze.rs::freeze_snapshot` — hash computation.
- `src/cli/serve.rs::serve_loop` — verify call site.
- `tests/cli_migration_e2e.rs::cli_migration_e2e_rejects_mismatched_wasm`
  — headlined end-to-end repro.