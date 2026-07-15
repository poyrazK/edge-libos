# ADR 0003 — P3 live migration

- **Status.** Proposed, 2026-07-15. Realized by P3 final-bundle PR on
  branch `worktree-p3-final-bundle`: `Subcommand::Migrate` variant in
  `src/cli/subcommand.rs`, `src/cli/migrate.rs` (freeze+serve subprocess
  wrapper), and `tests/migration_smoke.rs` (kernel-state roundtrip +
  AF_UNIX listener path preservation).
- **Phase.** P3 (per `impelementationplan` §7).
- **Scope.** The `edge-cli freeze <wasm> <snap>` → `edge-cli serve
  <wasm> <snap>` cross-host live-migration flow for v1, and the
  invariants the snapshot wire format MUST satisfy for that flow to
  be host-architecture-agnostic.

## Context

The snapshot wire format pinned by ADR 0002 §2 (explicit `LeU32` /
`LeU64` newtypes, host-native-endian forbidden at the call site)
implies that snapshots are byte-identical across x86 and ARM hosts —
no silent format divergence. P3 needs the actual **migration flow**
to honor that property: a snapshot frozen on host-A must restore
correctly on host-B (different CPU architecture), because the
guest's `.wasm` artifact is arch-agnostic and the kernel's
serialized state is fixed-width little-endian.

The motivating use case is "drain a hot instance from a failing x86
node to a healthy ARM node without dropping the in-flight request."
v1 is a strict subset of that goal: it covers freeze → copy →
serve, but NOT online drain-migration (the source stops serving
before the snapshot is taken). The deferred story is documented
here so a future PR knows the v1 boundaries.

P3 final-bundle adds the `Subcommand::Migrate` handler that
orchestrates the v1 flow as a single subprocess-driven command,
so an operator can `edge-cli migrate <wasm> <snap>` from host-A
without first manually running `freeze` and `serve` in separate
shells. The underlying freeze/serve handlers (D3.5 / D3.7) are
unchanged.

## Decision

Six concrete commitments that the migration flow MUST honor.

### 1. Module artifact portability — `wasm32` is arch-agnostic

A `wasm32` module compiled by `guest/build.sh` or `zig cc
-target wasm32-freestanding` runs identically on x86 and ARM hosts.
The host binary (`edge-cli`) instantiates it with the same
Wasmtime `Engine` config (modulo target-architecture-specific
codegen, which is transparent to the guest). The same `.wasm`
file works on both hosts — no cross-compile step is required at
migration time.

The `KernelSnapshot` is byte-identical across hosts because
ADR 0002 §2 mandates explicit `LeU32` / `LeU64` newtypes for every
numeric field. Host-native-endian serialization is a compile error
at the serde boundary, not a silent runtime divergence.

### 2. Drain semantics — v1 is freeze-then-serve, NOT online drain

The v1 migration flow is:

```
host-A$ edge-cli freeze <wasm> <snap>      # blocks until snapshot written
host-A$ scp <snap> host-B:
host-B$ edge-cli serve <wasm> <snap>       # blocks until guest exits
```

`apply_snapshot` is atomic from the guest's perspective: the
kernel-state apply (`apply_snapshot_kernel_state`) and the memory
apply (`apply_snapshot_to_memory`) happen back-to-back inside a
single synchronous call. There is no `.await` between them. From
the source host's perspective, freeze is a quiescent-point operation:
the source stops serving before the snapshot is taken, so no
in-flight request can be split across the snapshot boundary.

Online drain-migration (snapshot the live instance while it's
still serving, atomically swap the destination into the serving
role, copy the pages incrementally) is **explicitly out of scope
for v1** and is the subject of a future ADR if/when P3+ adds it.

### 3. Format-version interaction — `SNAPSHOT_FORMAT_VERSION` stays at 1

ADR 0002 §4 pins `SNAPSHOT_FORMAT_VERSION = 1` and mandates a
major-format bump for any backward-incompatible change. The P3
final-bundle adds no new fields to `KernelSnapshot`: `MemoryKind`
is a live-state field on `Kernel`, not part of the snapshot, and
`ChildExitStatus::waker` is dropped on freeze (it's a runtime
`std::task::Waker` handle, not serializable). The futex table
field was already added at the end-of-struct in P3 Tier-2 and
does not bump the version (ADR 0002 §4 exception: additive
end-of-struct fields don't bump).

A future migration-flow PR that adds a new field MUST either
keep the field additive-end-of-struct (no version bump) or
explicitly bump `SNAPSHOT_FORMAT_VERSION` to 2 and amend this
ADR.

### 4. `AcceptedStreamOnListener` handling — never constructed

`Resource::Socket::AcceptedStream` is a child of a listener fd.
On freeze, an accepted stream's remote peer is a host TCP/UNIX
connection whose state is not portable across hosts (the OS-side
socket buffers, sequence numbers, and kernel-side retransmit
queues live in the host kernel). An accepted stream cannot be
migrated — only a listener can.

The freeze CLI MUST refuse to snapshot a kernel that has any
`AcceptedStream` open: this is the `SnapshotError::AcceptedStreamOnListener`
variant (see `src/snapshot.rs::build_kernel_snapshot` and the
existing v1 contract that this variant is never-constructed). A
follow-up PR may add a fallback path (e.g., drain the stream
gracefully before freeze) — until then, freeze exits with
`Unsupported` and the operator must retry once the accepted
stream closes.

Abstract-namespace AF_UNIX sockets (`\0name`) are similarly
un-portable: their binding is to a host abstract namespace ID
that does not survive a host move. These abort the freeze CLI
with `SnapshotError::AbstractUnixNamespace` (existing variant).

### 5. v1 flow — `edge-cli migrate <wasm> <snap>`

The new `Subcommand::Migrate` handler in `src/cli/migrate.rs`
spawns the freeze and serve subcommands as subprocesses of the
same binary:

```rust
let exe = std::env::current_exe()?;
std::process::Command::new(&exe).arg("freeze").arg(&wasm).arg(&snap).status()?;
std::process::Command::new(&exe).arg("serve").arg(&wasm).arg(&snap).status()?;
```

This is intentionally a thin wrapper, not a re-implementation:
freeze and serve are the same code paths that the standalone
subcommands run, so the migrate subcommand inherits all of their
behavior (snapshot format, restore semantics, error reporting).
The wrapper exists so an operator can run a single command on
host-A when the destination host is the same machine (or to
document the intended sequencing).

Cross-host migration still requires the operator to `scp` (or
equivalent) the snapshot file between hosts — there is no
network transport in v1.

### 6. What this ADR does NOT cover

- **Online drain-migration.** Out of scope for v1. A future ADR
  must address incremental page copy, quiescent-point vs.
  stop-the-world semantics, and the swap-in protocol.
- **TCP connection migration.** A connected TCP socket's
  sequence-number state lives in the host kernel and is not
  portable across hosts. Even drain-migration cannot migrate
  an established TCP connection without kernel-bypass primitives
  (e.g., a userspace TCP stack). v1 migrates only listeners;
  v1.0+ freeze aborts on a connected socket.
- **`Module::serialize` cross-host.** wasmtime's compiled
  `Module` artifact (the result of `Module::serialize`) is
  host-arch-specific — x86 code is x86 code. Each host
  MUST re-compile its own `Module` from the `.wasm`. The
  `edge-cli serve` step handles this transparently: it calls
  `Module::new` (not `Module::deserialize`) on the destination.
  No code change is required for cross-host; the constraint
  is just that operators don't try to copy a serialized module
  artifact between hosts.
- **Migration of thread-local state.** The kernel's tokio
  task wakers are rebuilt on restore (per ADR 0002 §6); the
  destination host re-derives them. No special handling.
- **Migration of the clock anchor.** Per ADR 0002 §6,
  `started_at: Instant` is re-anchored to `Instant::now()` on
  the destination. A snapshot frozen at T0 on host-A and
  restored at T1 on host-B serves requests with T1-anchored
  monotonic time. This is the expected Linux `CLOCK_MONOTONIC`
  semantics for a live-migration scenario.

## Consequences

### What this ADR mandates on P3 final-bundle

- `src/cli/subcommand.rs` gains `Subcommand::Migrate` variant +
  `FromStr` mapping for `"migrate"`.
- `src/cli/migrate.rs` (new) implements the freeze+serve wrapper.
- `tests/migration_smoke.rs` (new) covers the kernel-state
  roundtrip (memory marker survives) and the AF_UNIX listener
  path preservation (reopen path from PR #15).
- No change to the snapshot wire format. No bump to
  `SNAPSHOT_FORMAT_VERSION`.

### What this ADR enables for follow-ups

- **Online drain-migration (P3+).** The atomic-apply guarantee
  from §2 is a prerequisite; once incremental copy lands,
  drain-migration is just "copy pages during the freeze
  quiescent-point window instead of stopping the source first."
- **Cross-architecture CI gate.** A CI job that snapshots on
  x86 and restores on ARM (or vice versa) is now a valid
  conformance test — the format invariants from ADR 0002 §2
  plus the §1 commitment above are sufficient.
- **Operator tooling.** `edge-cli migrate` can be wrapped in
  shell scripts, systemd units, or k8s lifecycle hooks without
  changing the kernel.

### What this ADR blocks

- The freeze CLI cannot silently accept an `AcceptedStream`
  open in the kernel. It MUST abort with
  `SnapshotError::AcceptedStreamOnListener`.
- The freeze CLI cannot silently accept an abstract-namespace
  AF_UNIX socket. It MUST abort with
  `SnapshotError::AbstractUnixNamespace`.
- The migration flow cannot be used to migrate a connected TCP
  socket. Operators must drain connected sockets before freeze.
- The `Module::serialize` artifact MUST NOT be copied across
  hosts. Operators must copy the `.wasm` and let each host
  re-compile.

## References

- `impelementationplan` §7 (P3 DoD — live migration listed as
  Tier-7 follow-up; v1 is the freeze-then-serve subset).
- `HANDOFF.md` §3.4 (P3 scope, live migration).
- ADR 0002 §2 (LeU32/LeU64 newtypes — the cross-arch
  guarantee).
- ADR 0002 §4 (format-version rule — additive end-of-struct
  fields don't bump).
- ADR 0002 §6 (what does NOT get serialized — clock anchor,
  wakers, Memory handle).
- ADR 0001 §2 (multi-fiber contract — Store is !Send/!Sync,
  per-fiber pinned; relevant if drain-migration ever spawns a
  child fiber for the child kernel).
- PR #15 (P2-D3 — AF_UNIX listener reopen path used by the
  unix-listener-preservation migration smoke test).
- PR #18 (P2-D3 — edge-cli subcommands, including the freeze
  and serve subcommands that Migrate wraps).
- PR #17 (P3 Tier-4 — clone v1, which inserts into
  `Kernel.children`; relevant to the snapshot rebuild loop).
- PR #19 (P3 Tier-6 — wait4 v1; the child-exit wake path that
  `Migrate`'s destination kernel inherits via
  `Kernel.child_event`).