# ADR 0002 — snapshot wire format

- **Status.** Accepted, 2026-07-14. P3 Tier-2 realized §5 by adding
  the `futex_table: Vec<FutexAddrSnapshot>` field to `KernelSnapshot`
  and threading it through `build_kernel_snapshot` /
  `apply_snapshot_kernel_state` (PR on branch `p3-t2-futex-snapshot`).
  The `Arc<Notify>` rebuild-on-restore contract from §5 is
  implemented in `FutexTable::rebuild_from_snapshot`
  (`src/sys/futex.rs`).
- **Phase.** P2-D (snapshot/restore) implements; P3 (fork via CoW,
  live x86→ARM migration) consumes.
- **Scope.** The on-disk / on-wire format that
  `edge-cli freeze` (P2-D1) writes and `edge-cli serve`
  (P2-D2) reads. P3 fork and P3 live migration MUST read the same
  format.

## Context

P2-D must serialize `Kernel` state + linear memory to enable the
1ms cold-start demo: boot the guest once at deploy, import
CPython + FastAPI + pydantic + the user's module, then snapshot.
Every request restores from the snapshot instead of re-running
imports.

P3 fork (`fork()` as CoW per `impelementationplan` §6) and P3 live
migration (drain a live instance x86→ARM per §7) both need to
consume the same wire format. If P2-D lands with native-endian
fields or a version-locked-on-first-write format, P3 has to
re-snapshot or re-migrate — a clean rebase hazard.

This ADR pins the format **before** P2-D lands so the P2 implementer
has a contract to honor. Any deviation requires an ADR amendment,
not silent format divergence.

## Decision

P2-D MUST produce a snapshot with the following properties.

### 1. Container — `postcard`

`postcard = "1"` (the only mandatory new crate dependency). Rationale:

- `no_std` capable — the kernel can deserialize in a constrained
  boot path if P3 ever needs it.
- Deterministic — no HashMap ordering surprises, no implicit
  floats, no `chrono` weirdness.
- Varint-encoded — small snapshots for typical kernels (single-digit
  MB).
- **No** version field by default; version handling is explicit
  (see §4).

If a follow-on ADR ever migrates away from `postcard`, this ADR
MUST be amended and a major-format-bump migration path documented.

### 2. Endianness — explicit `LeU*` newtypes

All multi-byte integers MUST be serialized via explicit little-endian
newtypes:

```rust
#[derive(Serialize, Deserialize)]
#[serde(transparent)]
pub struct LeU32(pub u32);

#[derive(Serialize, Deserialize)]
#[serde(transparent)]
pub struct LeU64(pub u64);
```

Rationale: a `#[derive(Serialize)]` on a raw `u32` field would
silently serialize host-native endian, which is x86 on the dev
machine and ARM on the migration target. Wrapping every numeric
field in `LeU32` / `LeU64` makes an accidental native-endian
serialization a compile error at the call site.

Use `to_le_bytes()` / `from_le_bytes()` at the serde boundary, not
`byteorder` or `zerocopy`. The newtype is the API.

### 3. Memory layout — sparse per-page

Linear memory is split into **fixed-size pages** (64 KiB each,
matching the wasm page size). Serialized as a length-prefixed
sequence of `(LeU32 page_index, Vec<u8> page_bytes)` pairs. Pages
the guest has never touched (zero-filled on the host side because
`wasmtime::Memory::data` returns zeros for unwritten pages) are
**omitted** from the snapshot — sparse representation.

```rust
// Wire format:
LeU32(format_version),          // see §4
LeU32(page_count),              // number of present pages
LeU32(page_index), LeU32(page_len), Vec<u8>(page_bytes),
...                             // repeated `page_count` times
LeU32(kernel_field_count),
(KernelSnapshot fields...),     // see §5
```

P3 fork uses the same per-page representation for CoW bookkeeping:
page-level dirty tracking, page-level copy on write. P3 live
migration streams pages one at a time so the source can keep
serving while the destination warms up.

### 4. Versioning — first byte is a `u8`

The very first byte of every snapshot is a `u8` format version.
Today's value: `FORMAT_VERSION: u8 = 1`. Bumping the version is
**mandatory** for any backward-incompatible change. The reader
MUST fail loudly with a clear error message (`"snapshot format
version 2 not supported (this build reads version 1)"`) rather
than silently producing a corrupt `Kernel`.

P3 implementers reading v1 snapshots MAY add a documented
migration path; they MUST NOT silently reinterpret an old
snapshot as a new one.

### 5. Kernel surface — explicit allowlist

Every field of `Kernel` either carries a comment `// SNAPSHOT:
include` or `// SNAPSHOT: skip (rebuilt on restore)`. Default is
**skip**. The include list for v1:

- `args: Vec<String>` — argv as the guest sees it.
- `env: Vec<(String, String)>` — environment as the guest sees it.
- `cwd: PathBuf` (in `vfs.cwd`) — restore path.
- `rng.state: [u8; 32]` — `SmallRng` seed material (P2-D snapshots
  the seed bytes, not the live RNG, so restore re-seeds).
- `exit_code: Option<i32>` — preserved across freeze/serve if set.
- `comm: [u8; 16]` — `prctl(PR_SET_NAME)` value (P2-C2).
- `fds: FdTable` — each open `Resource` enum variant serialized
  per its own contract (File: path + offset + flags; Socket:
  bound addr + listener state; Epoll: registered entries;
  EventFd: counter + flags; etc.).
- The linear-memory page map (§3).
- `futex_table` from ADR 0001 — `(u32 addr, u32 waiter_count)`
  pairs; `Notify` handles are rebuilt on restore.

### 6. What does NOT get serialized

- `Instant` clocks (`started_at`, `boot_monotonic_ns`,
  `ClockState`) — always re-anchored to `Instant::now()` /
  `SystemTime::now()` on restore. A snapshot frozen at T0 and
  restored at T1 serves requests with T1-anchored monotonic time.
- Thread-local observer state — `dispatch::OBSERVER` is process-
  per-thread; not part of the guest state.
- Tokio task wakers — always rebuilt; the snapshot only captures
  kernel-side state, not in-flight async tasks.
- `memory: Option<Memory>` — restored implicitly by the new
  wasmtime instance's `attach_memory`. The serialized memory
  bytes land in the page map; the `Memory` handle itself is
  derived from the wasmtime store after restore.

### 7. Pointers → offsets

Guest pointers in any kernel struct (e.g. an `iovec` saved in a
deferred read, or a saved `sigaction` handler address) MUST be
stored as raw `u32` offsets into the linear-memory page map.
On restore, the kernel re-validates each offset against the new
page map. Invalid offsets (out of range, in a hole) result in the
deferred op being silently dropped — NEVER a panic, NEVER a
`-EFAULT` returned to the guest for a snapshot-internal
inconsistency. The guest never sees the restore happen.

## Consequences

### What this ADR mandates on P2-D

- `Cargo.toml` adds `postcard = "1"` and (only if `Serialize`/`Deserialize`
  derives are wanted) `serde = { version = "1", features = ["derive"] }`.
- `LeU32` / `LeU64` newtypes live in `src/snapshot/types.rs` (or
  wherever P2-D puts snapshot code).
- Every `Kernel` field gets one of `// SNAPSHOT: include` /
  `// SNAPSHOT: skip (rebuilt on restore)` by the time P2-D lands.
- A `tests/snapshot_roundtrip.rs` integration test freezes a kernel
  with a known state, restores it, and asserts equality on the
  allowlist fields. This is the conformance gate for the format.

### What this ADR enables for P3

- `fork()` as CoW (P3-1) reuses the page-map representation
  directly. `parent.fd_table` and `child.fd_table` share `Arc`s
  where possible (File, Socket) per the P2-B5 shared-state pattern.
- P3 live migration (P3-last) streams the snapshot over the
  wire in the same format; the receiving side runs the same
  `tests/snapshot_roundtrip.rs` restoration path. Format changes
  are the same code path, not a parallel system.

### What this ADR blocks

- P2-D cannot use `bincode` (different defaults, defaults differ
  between versions; format ambiguity on `enum` representations).
- P2-D cannot use `serde_cbor` (canonical but CBOR's tagged values
  complicate the version byte).
- P2-D cannot store `Vec<HashMap<K, V>>` directly (non-deterministic
  ordering on serialize); use `Vec<(K, V)>` and sort by `K` first.

## References

- `impelementationplan` §6 (Snapshot / fork).
- `HANDOFF.md` §3.3 (P2 scope, snapshot/restore listed as
  "P2 scope, deferred to P2-D").
- `src/kernel.rs` (Kernel struct — the surface to snapshot).
- `src/fd.rs::Resource` enum (per-variant snapshot contracts).
- ADR 0001 (futex table — included in §5 allowlist).
- Memory: `wasm32-long-32bits` — `long` is 32 bits on wasm32-musl;
  pointer fields in snapshot structs MUST be `u32`, not `u64`.
