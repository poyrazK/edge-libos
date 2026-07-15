# ADR 0004 — freeze / serve wire contract (P2-D3.5)

- **Status.** Proposed, 2026-07-15. Realized by P2-D3.5 on branch
  `worktree-p3-d3-5-freeze-serve`: new `NR_SNAPSHOT = 123`
  syscall, `edge-cli freeze` + `serve` real bodies, fd-inherit
  plumbing on `Kernel`, and `edge-cli migrate` reworked to the
  subprocess flow.
- **Phase.** P2-D3.5 (per `impelementationplan` §7 — production-ish
  single instance; freeze/serve follow-up reserved in P2-D3.3).
- **Scope.** The wire-level contract between the four actors of
  the live-migration / production-serve flow:
  1. The **guest** (CPython et al.) — drives quiescence by calling
     `NR_SNAPSHOT`.
  2. The **`edge-cli freeze` host subprocess** — runs the guest,
     captures the snapshot, writes it to disk.
  3. The **`edge-cli serve` host subprocess** — restores the
     snapshot, attaches inherited listener fds, runs the guest to
     completion.
  4. The **`edge-cli migrate` host wrapper** — spawns the freeze
     + serve subprocess pair via `Command::new(current_exe)`
     (P3 final-bundle's in-process body is replaced; the in-process
     body is preserved as a `MIGRATE_IN_PROCESS=1` test opt-in).

## Context

P2-D3.3 (PR #18) landed `edge-cli freeze` and `edge-cli serve` as
**stubs** that return `CliError::Args("not yet implemented (lands
in D3.5)")`. P3 final-bundle (PR #21) added `edge-cli migrate` as
an **in-process** wrapper because the freeze/serve bodies were
not yet real (an operator on the same host would get a clean
roundtrip without needing a subprocess shell; the migration
contract was honored; the wire format was exercised). v1 ran the
whole flow in one process for the same reason.

That arrangement is no longer sufficient for P2 production-ish
single-instance: production deployment needs `serve` to attach a
pre-opened TCP listener fd from the parent (systemd-style socket
activation), and migration needs to spawn the real freeze +
serve subprocess pair so a cross-host (`scp`) replay exercises
the same code path as a same-host migrate. D3.5 closes the gap.

The motivating use cases:

1. **Systemd-style socket activation.** A host-level supervisor
   (`systemd`, `runit`, a k8s admission hook) binds the listening
   socket at boot time, then exec's `edge-cli serve` and tells
   it (via env vars) which fds to attach. The libOS must accept
   the inherited fd and forward `accept4` calls on it without
   re-binding.
2. **Cross-host migration via `scp`.** An operator freezes on
   host-A, copies the snap file, runs `serve` on host-B with the
   `.wasm` and the listener fd (now bound on host-B). The
   `.wasm` is arch-agnostic; the snap file is byte-identical
   across hosts per ADR 0002 §2.
3. **Guest-driven quiescence.** A long-running guest (CPython
   serving an HTTP request) is the actor that knows it just
   finished a request. The kernel gives the guest a syscall
   (`NR_SNAPSHOT`) to opt into being snapshotted. The host
   process does not need an out-of-band signal mechanism for
   the v1 flow — the guest drives quiescence when it's ready.

D3.5 is the v1 of this contract. SIGUSR1-driven quiescence,
online drain-migration, and TCP connection migration are
explicit follow-ups (§5).

## Decision

Five concrete commitments that v1 of the freeze/serve/migrate
flow MUST honor.

### 1. Quiescent point — guest-driven via `NR_SNAPSHOT = 123`

The guest is the actor that knows when it is at a quiescent
point. The kernel exposes a new syscall:

```c
// src/sys/process.rs::NR_SNAPSHOT == 123
//
// arg0 (i64): pointer (in guest linear memory) to a
//             null-terminated absolute path string.
long snapshot(const char *snap_path);
```

When the guest calls `NR_SNAPSHOT`, the kernel:

1. Reads the path string from guest linear memory via
   `crate::mem::guest_str` (existing EFAULT choke point).
   Returns `-EFAULT` on a bad pointer.
2. Encodes the live kernel via
   `crate::snapshot::try_to_snapshot` +
   `crate::snapshot::encode_snapshot` (existing snapshot path).
3. Writes the encoded bytes to the path via `std::fs::write`.
   Returns the number of bytes written on success; `-EIO` on
   write failure.
4. Returns the byte count to the guest (>= 0 is the standard
   kernel-style success convention).

v1 supports path-based output only. fd-based output (an
`out_fd: i32` argument that writes via `write(2)` to the fd) is
deferred — the v1 subcommand uses regular files on tmpfs, which
is the cross-host transport for `migrate`. SIGUSR1-driven
quiescence is §5 out-of-scope.

The host subprocesses (`edge-cli freeze` and `serve`) do NOT
read the path argument — the guest's syscall writes directly
to the path. The host's only coordination with the guest is
the engine / linker / `Store` plumbing that lets the guest run.

Why this shape and not "the host signals the guest"?

* **`std::process::Command::new` does not give us a
  per-guest-fd signal channel.** SIGUSR1 delivered to the host
  process must be turned into a guest-visible signal through
  the existing `Kernel.signal_state` table — but signal
  delivery to a multi-fiber guest (ADR 0001 §2: `Store: !Send`)
  needs a per-fiber delivery story that doesn't exist yet.
  Building that for v1 to gate the freeze flow on is the wrong
  order.
* **`edge-cli trace` and the C conformance runner already
  test the syscall directly.** Driving quiescence from inside
  the guest exercises the same code path; no separate signal
  deliverer is needed.
* **`migrate` already waits for `_start` to return.** The
  host can fall back to "snapshot after `_start` returns 0"
  for legacy guests that don't call `NR_SNAPSHOT` themselves,
  matching the existing `migrate` behavior.

### 2. fd-inherit shape — `EDGE_SERVE_FD_<N>` env vars

`edge-cli serve <wasm> <snap>` reads the environment at process
startup. For each `EDGE_SERVE_FD_<N>` env var found (where
`<N>` is an unsigned decimal starting at 0), the binary
parses the value as an ASCII decimal fd number, wraps the fd
in a `tokio::net::TcpListener` via
`std::net::TcpListener::from_raw_fd` (after `dup`'ing the fd so
the host process can continue to hold it), and attaches each to
the kernel as a `Resource::Socket` BEFORE calling `_start`, so
the guest's first `accept4` syscall on the inherited fd number
returns a connected stream without any `bind(2)` /
`listen(2)` interaction.

Env var contract:

* `EDGE_SERVE_FD_0` is the canonical inherited listener 0.
* `EDGE_SERVE_FD_<N>` for `N >= 1` are additional inherited
  listeners, in ascending N order. Operators are responsible
  for setting them in order — gaps (e.g. `FD_0=4`, `FD_2=5`)
  are ignored past the first gap.
* Each value is an ASCII decimal fd number, base 10, no
  formatting. A non-numeric value is a hard error: the serve
  subcommand exits 2 with a clear error message naming the
  offending var. This is operator error, not user error, and
  must fail loudly.
* An absent `EDGE_SERVE_FD_0` is **legitimate** — a guest that
  doesn't need an inherited listener (e.g. a one-shot batch
  job) can be served without inheriting any fd. The serve
  subcommand proceeds with zero inherited listeners.

The kernel-side plumbing is new on `Kernel`:

```rust
// src/kernel.rs
pub fn attach_inherited_listeners(&mut self, fds: Vec<i32>) {
    // For each fd: std::net::TcpListener::from_raw_fd(clone),
    //              wrap in Resource::Socket (SocketInner
    //              derived from getsockname(addr)), insert
    //              into self.fds at the fd number.
}
```

This is called by `edge-cli serve` AFTER `apply_snapshot` (so
the inherited fd doesn't get clobbered by a re-bind) but
BEFORE `_start` (so the guest's first `accept4` sees the
inherited fd).

The inherited fd numbers need not be sequential from 0 — the
inherited fd may have any number. The next available fd for
the kernel's internal use is `fds.iter().max().unwrap_or(2) +
1`, which the kernel chooses when allocating new fds via
`open(2)`, `socket(2)`, etc. (stdin/stdout/stderr are 0/1/2 in
the inherited set if present; the kernel never implicitly
opens 0/1/2 on the guest's behalf.)

Lock discipline: `parking_lot::Mutex` on `self.fds` (already
in place from earlier milestones; never held across `.await`).

### 3. Subprocess wire format — `edge-cli migrate`

`edge-cli migrate <wasm> [--] [args...]` is the production-shape
operator entry point. v1 invokes the freeze + serve subprocesses
via `Command::new(std::env::current_exe()?)`:

```rust
let exe = std::env::current_exe()?;
let snap_path = std::env::temp_dir().join(format!(
    "edge-migrate-{}-{}.snap", std::process::id(), uuid
));
let freeze = Command::new(&exe)
    .arg("freeze").arg(&wasm).arg("--out").arg(&snap_path)
    .status()?;
if !freeze.success() { return Err(...); }
let serve = Command::new(&exe)
    .arg("serve").arg(&wasm).arg(&snap_path)
    .status()?;
serve.code().unwrap_or(2)
```

The snapshot bytes travel via a regular file path argument
**deliberately**, not via stdin/stdout pipes to the subprocess:

* Pipes require the source process to keep running past the
  freeze call so the destination can read them. A regular
  file decouples the two: `freeze` writes the file and exits;
  the operator `scp`s the file to host-B; `serve` reads it.
* `migrate` deletes the temp snap file on both success and
  error paths before returning the propagate exit code.
* The C conformance runner + integration tests use the
  in-process path via `MIGRATE_IN_PROCESS=1` env var (faster,
  no subprocess overhead, lets fixtures assert specific
  state). The default `migrate` is subprocess.

Cross-host migration still requires the operator to `scp` (or
equivalent) the snapshot file between hosts — there is no
network transport in v1.

### 4. Snapshot format version — stays at `1`

`NR_SNAPSHOT` is a guest-to-host **message** (a syscall), not a
wire-format change. It does not appear in the snapshot bytes
themselves.

The new `inherited: bool` field on `SocketSnapshot`
(src/snapshot.rs) is added at the **end-of-struct** of the
existing snapshot type, behind `#[serde(default)]`. Per ADR
0002 §4 (additive end-of-struct fields don't bump the
version), the version stays at `SNAPSHOT_FORMAT_VERSION = 1`.
A snapshot frozen before the `inherited` field was added
decodes cleanly even without the field — `serde(default)`
fills it with `false`.

### 5. Out of scope (deferred follow-ups)

1. **SIGUSR1-driven quiescence.** The host process listens for
   SIGUSR1 and converts it to a queued `SIGSNAPSHOT` signal
   that the guest traps via the existing `rt_sigaction` path.
   v1 quiescence is guest-driven only. SIGUSR1 needs a
   per-fiber signal-delivery story (ADR 0001 §2: `Store:
   !Send`) that doesn't exist yet; landing it requires the
   signal-aware `wait4` follow-up first.
2. **Online drain-migration (ADR 0003 §5).** Snapshotting a
   live instance while it serves, atomically swapping the
   destination into the serving role, copying pages
   incrementally. v1 is freeze-then-copy-then-serve; the
   source stops serving before the snapshot is taken.
3. **TCP connection migration (ADR 0003 §6).** A connected
   TCP socket's sequence-number state lives in the host
   kernel and is not portable. Even drain-migration cannot
   migrate an established TCP connection without
   kernel-bypass primitives (e.g., a userspace TCP stack).
   v1 migrates only listeners; v1.0+ freeze aborts on a
   connected socket (existing behavior from P2-D3.2 +
   `SnapshotError::AcceptedStreamOnListener`).
4. **`NR_SNAPSHOT(out_fd)` fd-arg variant.** The current
   path-based argument is the v1 contract; an out_fd
   argument for SCM_RIGHTS-passed sockets is a follow-up if
   a future use case needs it.
5. **`edge-cli bench` real body** (D3.7 follow-up). Still a
   stub from P2-D3.3.

## Consequences

### What this ADR mandates on P2-D3.5

- New `src/sys/process.rs::snapshot_syscall` + `NR_SNAPSHOT`
  const, with one dispatch arm in `dispatch::dispatch` and
  one in `dispatch::syscall_name`.
- New `Kernel::attach_inherited_listeners(Vec<i32>)` method
  + new `Socket::from_inherited_listener` constructor on
  `fd.rs`.
- New `inherited: bool` (with `#[serde(default)]`) on
  `SocketSnapshot` in `src/snapshot.rs::try_to_snapshot` and
  the apply path.
- New `tests/conformance/snapshot.c` (marker-enforced),
  `tests/snapshot_syscall_conformance.rs` (2 Rust tests),
  `tests/inherit_listener_conformance.rs` (2 Rust tests),
  `tests/cli_freeze_smoke.rs` (2 tests),
  `tests/cli_serve_smoke.rs` (3 tests),
  `tests/cli_migration_e2e.rs` (1 end-to-end test).
- Real bodies in `src/cli/freeze.rs` and `src/cli/serve.rs`
  (replacing the D3.3 stubs).
- `src/cli/migrate.rs::run_main` replaced with subprocess
  shape; in-process body preserved as `run_main_in_process`
  behind `MIGRATE_IN_PROCESS=1` env opt-in for tests.
- `tests/migration_smoke.rs::migration_smoke_subprocess_roundtrip`
  un-`#[ignore]`d (now fast enough to run on every push).
- `tests/conformance/runner.sh` gains `snapshot.c`.
- `HANDOFF.md` regen: Rust 197→211, C 105→106, Grand
  425→445. `README.md` totals bump + D3.5 note.
- `Cargo.toml` keeps `version = "0.2.0"` (no version bump;
  P3 is closed and D3.5 is a follow-up, not a release).

### What this ADR enables for follow-ups

- **SIGUSR1-driven quiescence** (§5.1) reuses `attach_inherited_listeners`
  + the syscall variant of `apply_snapshot`; adding it is a
  small incremental change on top of D3.5.
- **Online drain-migration** (§5.2) reuses the freeze/serve
  pair; the substantive new work is incremental copy +
  quiescent-point semantics, not the wire contract.
- **Cross-architecture CI gate for migrate.** A CI job that
  builds `.wasm` once, freezes on x86, serves on ARM (or
  vice versa) with the same snap file becomes valid — the
  format invariants from ADR 0002 §2 hold.
- **Systemd-style deployment.** `systemd` units with
  `FileDescriptorName=EDGE_SERVE_FD_0` map directly onto the
  env-var contract in §2. No kernel change needed beyond
  `attach_inherited_listeners`.

### What this ADR blocks

- The `freeze` subcommand cannot silently drop a guest's
  `NR_SNAPSHOT` request — it must surface the byte count and
  any write errors.
- The `serve` subcommand cannot re-bind an inherited listener
  fd in the apply path — the apply path's existing "fd is
  already bound" detection is bypassed for inherited fds
  (handled by `SocketSnapshot::inherited: true`).
- The `migrate` subcommand cannot silently swallow a
  non-zero freeze or serve exit code — exit codes propagate.
- The `edge-cli` binary cannot swallow `EDGE_SERVE_FD_<N>`
  parse errors — they are operator errors and must fail
  loudly with exit 2.

## References

- `impelementationplan` §7 (P2 DoD — D3.5 listed as the
  freeze/serve follow-up to D3.3).
- ADR 0001 §2 (multi-fiber contract — `Store: !Send`,
  per-fiber pinned; relevant to SIGUSR1-driven quiescence
  once a per-fiber signal-delivery story exists).
- ADR 0002 §2 (LeU32/LeU64 newtypes — cross-arch wire
  guarantee).
- ADR 0002 §4 (format-version rule — additive end-of-struct
  fields don't bump).
- ADR 0002 §6 (what does NOT get serialized — clock anchor,
  wakers, Memory handle).
- ADR 0003 §2 (drain semantics — v1 is freeze-then-serve,
  not online drain).
- ADR 0003 §6 (what this ADR does NOT cover — re-stated for
  cross-reference).
- P2-D3.2 (TCP listener reopen pattern mirrored by
  `Socket::from_inherited_listener`).
- P2-D3.3 (PR #18) — the D3.3 stub bodies being replaced.
- PR #21 (P3 final-bundle) — the in-process migrate body
  being replaced.
- systemd socket activation (sd_listen_fds / LISTEN_FDS /
  LISTEN_PID) — the reference contract for §2's env-var
  shape.
