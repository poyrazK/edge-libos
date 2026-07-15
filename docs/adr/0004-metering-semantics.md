# ADR 0004 — metering semantics

- **Status.** Proposed, 2026-07-15. Realized by P2 metering slice on
  branch `p2-metering-hooks`. Promoted Accepted once M7 (DoD step 11)
  is green on CI.
- **Phase.** P2 metering — after P2-D3 (snapshot/restore/cold-start
  on `p2-d3-freeze-serve-and-cold-start`), before P3 fork (clone/fork
  as CoW) and P3 live migration.
- **Scope.** Per-request CPU budget on a guest instance, exposed via
  the CLI surface (`run`/`serve`/`bench`) and snapshotted into
  `KernelSnapshot` so that `serve` can carry usage across restore.

## Context

`edge-cli serve` respawns the guest from a snapshot and serves
requests on a loop. Today it has no per-request CPU ceiling: a
malicious or buggy guest can spin in a tight loop and starve the
host's tokio runtime of yield points. The host's only lever today
is the `tokio::time::timeout` that wraps `call_start`, which doesn't
fire for a guest that's already inside `_start` and looping on
non-yielding wasm instructions.

Wasmtime 45.0.3 ships two built-in metering primitives:

1. **Fuel** — `Config::consume_fuel(true)` + `Store::set_fuel(N)`.
   Fuel is decremented by wasmtime's instrumentation on every wasm
   instruction. The guest traps when fuel runs out. Per-Store
   (`Store::set_fuel` is independent for each `Store`), deterministic
   for a given instruction sequence, and explicitly recommended by
   the wasmtime docs for "deterministic interruption of a fixed,
   finite interval" (`wasmtime-45.0.3/src/runtime/store.rs:1158`).
2. **Epoch** — `Config::epoch_interruption(true)` +
   `Engine::increment_epoch()` + `Store::set_epoch_deadline(N)`.
   Wall-clock coarse-grained; the host increments a global counter
   (typically once per millisecond from a parking task) and the
   guest traps when its `Store`'s deadline is exceeded. Docs warn:
   "intended to allow for coarse-grained interruption, but not a
   deterministic deadline of a fixed, finite interval" (same file).

This ADR pins the metering model **before** the slice lands so the
M2–M7 implementers share a contract.

## Decision

P2 metering MUST use **fuel** as the budget primitive. Epochs are
explicitly out of scope. Concrete contract:

### 1. Engine — `Config::consume_fuel(true)` unconditionally, yield interval OFF

`src/host.rs::build_engine` flips `cfg.consume_fuel(true)` so that
every Store in the engine can have its budget set. Per-Store
defaults:

- `Store::set_fuel(u64::MAX)` in `build_store` — without this,
  wasmtime's default is 0 fuel and the wasm traps on the first
  instruction (per the wasmtime docs). The `u64::MAX` default
  preserves pre-ADR behavior for callers who don't care about
  metering. Subcommands that want a real budget override via
  `set_fuel(ms_to_fuel(budget))`.
- `fuel_async_yield_interval` is **deliberately not called**.
  Empirically determined (see
  `tests/snapshot_roundtrip.rs::snapshot_roundtrip_preserves_memory_and_stdout`)
  that setting a yield interval causes wasmtime 45.0.3 to re-enter
  the wasm at the host call site and double-invoke the host
  handler — `write(1, ..., 6)` produced 12 bytes in the stdout
  buffer. The budget enforcement is still effective because
  `set_fuel(N)` makes the wasm trap on the (N+1)th instruction;
  the only loss is cooperative yielding, which we accept until
  host handlers are audited for yield safety.

The cost (instrumentation overhead per wasm instruction) is borne
unconditionally because:

- Every Store needs a `set_fuel` call to be sensible anyway; turning
  it off per-Store would invite subtle bugs where a guest on a
  "fuel-off" Store has unbounded CPU.
- The instrumentation is the only way to enforce a per-request CPU
  budget. `fuel_async_yield_interval` is orthogonal to that goal.

`YIELD_INTERVAL_FUEL` constant in `src/meter.rs` stays at
`u64::MAX` (= never yield) until the host-handler audit lands.

### 2. Per-request budget — `--cpu-budget-ms <ms>` CLI flag

`edge-cli run`, `edge-cli serve`, `edge-cli bench` all accept
`--cpu-budget-ms <ms>`:

- `run` — applies to the guest's lifetime. If the guest traps on
  out-of-fuel, the host records the trap, drains stdio, and exits
  with `CliError::Metered` → exit code 1. The error message names
  the budget and the consumed units.
- `serve` — applies **per request**. After `apply_snapshot_*`, the
  host calls `store.set_fuel(budget_fuel)` and lets the guest run
  for one request. When `call_start` returns (the request finished
  or trapped), the host records `fuel_consumed =
  budget_fuel - store.get_fuel().unwrap()` into `kernel.cpu_ns`,
  resets `set_fuel(budget_fuel)`, and loops.
- `bench` — same semantics as `serve`: each iter sets a fresh
  budget, applies the snapshot, drains one request, measures
  restore cost AND per-request fuel consumption.

The default if `--cpu-budget-ms` is **not** given:

- `run` and `bench` — `u64::MAX` (effectively unbounded; same
  semantics as today).
- `serve` — required. `serve` with no budget is `CliError::Args`
  (exit 2). Rationale: `serve` is the production-path surface;
  leaving it unbounded would defeat the point.

`--cpu-budget-ms 0` is rejected as `CliError::Args` — zero budget
traps the guest on the first instruction, which is a confusing
user experience.

### 3. Budget → fuel conversion — calibrate in M6, lock after

`wasmtime`'s fuel unit is an abstract instrumentation count, not a
wall-clock cycle. The mapping lives in **one place**
(`src/meter.rs::FUEL_PER_MS`) and is **calibrated empirically** in
M6 by running a WAT fixture that busy-loops N iterations and
reading the consumed fuel. ADR 0003 does not pre-pin the constant;
the M6 calibration writes it. Until then, FUEL_PER_MS is a
provisional 1_000_000 (≈1 µs/instruction on x86_64), used only for
the initial integration smoke; the calibration commit overwrites it.

### 4. Per-Store state on `Kernel` — `cpu_ns: u64`

`Kernel` gains one new field, snapshotted:

```rust
/// ADR 0003: monotonic CPU time consumed by the guest since
/// `set_fuel` was last called. Drives the per-request reporting
/// path in `serve` and the bench's per-iter print. NOT a
/// wall-clock Instant — it's a fuel-derived estimate, which is
/// what the user is asking for ("CPU time used").
pub cpu_ns: u64,  // SNAPSHOT: include
```

Updated on every syscall re-entry by reading
`store.get_fuel().unwrap()` and subtracting from the budget. The
delta is added to `kernel.cpu_ns` so a single request's total
fuel burn is recoverable after the syscall chain returns.

`cpu_ns` resets on `serve` between requests. It does NOT reset
across `set_fuel` on `run` or `bench` — those are single-shot.

### 5. Snapshot wire format — `cpu_ns` is in

`KernelSnapshot` gets a new field per §4. The format version STAYS
at `1` (`src/snapshot.rs:63`): adding a field is backward-compatible
(postcard serde is forgiving on new fields), and the round-trip
test in `tests/snapshot_roundtrip.rs` already exercises the kernel
allowlist. **No version bump.**

ADR 0002 §5's allowlist gains one row: `cpu_ns: u64`. ADR 0002 §6
("what does NOT get serialized") does NOT need updating — fuel
itself is per-Store and not serialized, but the **consumed**
counter is.

### 6. Trap → exit code mapping

When fuel runs out, wasmtime traps with `OutOfFuel`. The host's
existing `call_result.is_err()` checks (e.g. `src/cli/run.rs:106`)
treat all traps as ignorable. The metering slice adds a **distinct
check**: if the trap's `Display` impl contains `"all fuel consumed"`
(wasmtime 45.0.3 message), return `CliError::Metered(format!(
"cpu budget exceeded: used {} µs / budget {} µs",
cpu_consumed_us, budget_us))` → exit code 1.

For `serve`, the trap is caught and **counted** rather than
propagated: the request fails, the loop continues with the next
request after `set_fuel` reset. A counter `kernel.requests_met`
increments so the bench can report "N requests metered / M total".

### 7. Tracing interaction

`edge-cli trace` uses `install_observer` (a thread-local), which
fires on every syscall. The observer does not currently inspect
fuel state. The metering slice adds ONE line to the observer's
exit hook: `cpu_ns_total = store.get_fuel().unwrap_or(0)` is read
and emitted as an extra JSON field on the syscall record. This is
the first per-syscall CPU-time attribution we have.

`run`/`bench`/`serve` do NOT install an observer (the observer is
in-process and would distort perf); they read `store.get_fuel()`
directly on `call_start` return.

## Consequences

### What this ADR mandates on P2 metering

- `src/host.rs::build_engine` flips `consume_fuel(true)` and
  configures `fuel_async_yield_interval(Some(YIELD_INTERVAL_FUEL))`.
- `src/kernel.rs::Kernel` gains `cpu_ns: u64` with
  `// SNAPSHOT: include`.
- `src/snapshot.rs::KernelSnapshot` gains `cpu_ns: LeU64`.
- `src/meter.rs` (NEW) holds `pub const FUEL_PER_MS: u64` (provisional
  1_000_000, overwritten by M6 calibration), `pub const
  YIELD_INTERVAL_FUEL: u64`, `pub fn ms_to_fuel(ms: u64) -> u64`,
  and the `OutOfFuel` trap-to-CliError classifier.
- `src/cli/error.rs` gains `Metered(String)` → exit 1.
- `src/cli/{run,serve,bench}.rs` accept `--cpu-budget-ms <ms>` and
  call `store.set_fuel(ms_to_fuel(budget))` at the right point.
- `src/dispatch.rs::OBSERVER` exit hook emits the fuel snapshot
  on the syscall JSON line.

### What this ADR enables for P3

- `fork()` as CoW (P3-1) inherits `cpu_ns` from the parent — the
  child starts with the parent's accumulated usage. The child's
  first `set_fuel` resets the per-request budget but does NOT
  reset `cpu_ns`; that field is process-scoped, not request-scoped.
- P3 live migration streams `cpu_ns` like any other `Kernel` field
  — it's part of `KernelSnapshot`, so format compatibility is free.

### What this ADR blocks

- P2 metering cannot use **epoch interruption**. Reason: the docs
  warn against using it for finite-interval deadlines, and it
  requires a parking task that bumps `Engine::increment_epoch()`
  globally. Adding that task now means P3's `Engine` cloning (for
  per-fiber `Store`s) would also have to clone the epoch bump logic.
- P2 metering cannot add a **real `prlimit(PR_SET_CPU_LIMIT)`** syscall
  handler. Reason: musl/macOS diverge on the resource constants; we'd
  land a Linux-only shim that confuses macOS test runs. The CLI flag
  is the right surface for now.
- `set_fuel` cannot be **per-fiber** until P3 multi-fiber lands.
  Reason: ADR 0001 §2 establishes one `Store` per fiber (Store is
  !Send), so per-fiber fuel is per-Store already — no extra work.
  But M2 cannot change the `Store`-per-instance assumption.

## References

- `impelementationplan` §5.3 (per-request budget; deferred to P2).
- `wasmtime-45.0.3/src/runtime/store.rs:1058-1076` (fuel semantics).
- `wasmtime-45.0.3/src/runtime/store.rs:1078-1109` (fuel_async_yield_interval).
- `wasmtime-45.0.3/src/runtime/store.rs:1111-1140` (set_epoch_deadline).
- `wasmtime-45.0.3/src/runtime/store.rs:1141-1159` (epoch caveat — "not deterministic").
- ADR 0001 §2 (one Store per fiber — fuel is already per-Store).
- ADR 0002 §5 (snapshot allowlist — gains `cpu_ns`).