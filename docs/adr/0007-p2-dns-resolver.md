# ADR 0007 — P2-DNS: project-private NR_RESOLVE syscall

## Status

Accepted (2026-07-15). Implemented across commits `b8e32f2`–`cf4ba1d`
on branch `worktree-p2-dns-resolver`.

## Context

DNS resolution is needed for FastAPI / httpx / SQLAlchemy-over-Postgres
workloads — the P2 DoD. The current `main` branch (490 tests:
384 Rust + 106 C) has all the syscall surface and the snapshot path,
but **a CPython guest cannot resolve hostnames**, so httpx and
SQLAlchemy cannot reach any real service. DNS is the only true
production blocker; everything else is follow-up.

Three load-bearing facts reframed the obvious plan:

1. **There is no Linux `getaddrinfo` syscall at any NR.** NR 63 is
   already `uname` ([`src/sys/identity.rs:19`](../../src/sys/identity.rs:19)).
   musl implements `getaddrinfo(3)` inside libc by issuing regular
   `socket`/`sendto`/`recvmsg`/`poll` syscalls over UDP — confirmed by
   reading upstream `musl/src/network/getaddrinfo.c` and
   `arch/x86/entry/syscalls/syscall_64.tbl`.
2. **No UDP at all in the kernel today.** `SocketKind::Datagram` exists
   as an enum tag ([`src/sys/socket.rs:117`](../../src/sys/socket.rs:117)),
   but `sendto`/`recvfrom` are TCP-only. No `tokio::net::UdpSocket`
   anywhere in `src/`. So the obvious path ("let musl's UDP DNS run
   over our sockets") is **architecturally dead** until we add UDP —
   a separate, much larger workstream.
3. **musl `EAI_*` are negative** (`EAI_NONAME = -2`), not positive. The
   wasm32 `struct addrinfo` is **32 bytes** (4-byte pointers), not 48.

Three paths were considered:

- **Path A — implement UDP socket layer + let musl resolve.** Multi-PR
  workstream: real `UdpSocket`, `sendto`/`recvmsg` honoring guest
  `msghdr`/`iov`, `/etc/hosts` + `/etc/resolv.conf` VFS files,
  `__res_msend` over UDP. Months, not a milestone.
- **Path B (chosen) — project-private `NR_RESOLVE` syscall + guest libc
  adapter.** Carries the semantics of `getaddrinfo(3)`. A tiny
  `guest/resolver/` adapter overrides musl's `getaddrinfo` and
  `freeaddrinfo` symbols to marshal through the new NR.
- **Path C — re-implement getaddrinfo entirely on the guest side using
  existing syscalls.** Guest is wasm32-musl with
  `--disable-threads --without-threads --disable-ipv6 --disable-ssl`
  ([`guest/build.sh:72-79`](../../guest/build.sh:72)). No asyncio
  executor, no IPv6, no TLS. Host-side resolution is faster and more
  testable.

## Decision

Path B:

### NR choice

- **`NR_RESOLVE = 400`** in the upstream-reserved range 387-423 (per
  `arch/x86/entry/syscalls/syscall_64.tbl` header — "don't use numbers
  387 through 423, add new calls after the last 'common' entry").
  Inside that range is guaranteed safe.
- **`NR_SNAPSHOT`'s docstring** ([`src/sys/process.rs:92-97`](../../src/sys/process.rs:92))
  is fixed to reflect the truth: both project-private NRs (123 and
  400) live in reserved/adjacent space, not "unused upstream NRs".
  Future project-private syscalls should also live in 387-423.

### Wire contract — `NR_RESOLVE`

```
NR_RESOLVE = 400

a[0] = node_ptr      (u32 guest ptr; 0 = no node)
a[1] = node_len      (i64; 0 = scan to NUL; cap 256)
a[2] = service_ptr   (u32 guest ptr; 0 = no service)
a[3] = service_len   (i64; 0 = scan to NUL; cap 64)
a[4] = hints_ptr     (u32 guest ptr; 0 = no hints)
a[5] = res_ptr_ptr   (u32 guest ptr to a u32 slot; handler writes head)

Return:
  >= 0   success; number of addrinfo nodes written
  <  0   -EAI_* (musl-negative: -1 BADFLAGS, -2 NONAME, -3 AGAIN,
                 -4 FAIL, -6 FAMILY, -7 SOCKTYPE, -8 SERVICE,
                 -10 MEMORY, -11 SYSTEM, -12 OVERFLOW)
```

The guest adapter returns the syscall return value cast to `int`
directly — both sides share the negative EAI space, no translation.

### Per-`ProcessState` placement

```rust
pub resolver: parking_lot::Mutex<ResolverState>,
```

Mirrors `futex_table` (ADR 0006). Shared across `clone`/`fork` via
`Arc<ProcessState>`. Field defaults: empty cache, no denylist,
60_000 ms TTL, 5_000 ms timeout.

### `ResolverBackend` trait for testability

```rust
#[async_trait::async_trait]
pub trait ResolverBackend: Send + Sync {
    async fn lookup_ip(&self, name: &str) -> anyhow::Result<Vec<IpAddr>>;
}
```

Production: `TokioResolverBackend` wraps `hickory_resolver::TokioResolver`,
lazy-built on first call. Test: `StubResolver` returns a hardcoded
`Vec<IpAddr>`, optional sleep for timeout tests. Chosen over
"real-DNS-only tests" because deterministic offline integration tests
are worth a 40-LOC trait.

### Snapshot non-persistence

`KernelSnapshot` does **not** include `ResolverState`. Documented in
[`src/snapshot.rs`](../../src/snapshot.rs) field comments. On
`apply_snapshot`: denylist default (operator re-issues env vars on
`serve`), cache empty, backend None (rebuilt on first lookup). Mirrors
how other process-local state is handled — rebuild is cheaper than
serializing hickory internal pools.

### EAI sign convention

Negative on the wire (matches musl). No translation in the adapter.

### Guest-side adapter

`guest/resolver/{getaddrinfo,freeaddrinfo}.c` override musl's weak
symbols via link-order. Each `getaddrinfo` call:

1. Copies `node` and `service` into a local scratch region in linear
   memory at known offsets after `MARKER_ADDR` (so the host reads
   from a deterministic guest-visible region, not from arbitrary
   caller pointers).
2. Calls `edge_host_lookup()` → `NR_RESOLVE`.
3. On success, walks the host-written linked list, `malloc`s each
   `addrinfo` node + each `ai_addr`, and re-points `ai_next` into the
   musl-owned chain.
4. `freeaddrinfo` walks + frees.

### Operator plumbing

`EDGE_RESOLVER_DENY=<ip>,<ip>`, `EDGE_RESOLVER_CACHE_TTL_MS=<n>`,
`EDGE_RESOLVER_TIMEOUT_MS=<n>`. Parsed in
`ResolverConfig::from_env`, attached via `Kernel::attach_resolver_config`
in `src/cli/run.rs` and `src/cli/serve.rs`.

## Consequences

- New module `src/sys/resolver.rs` (handler + helpers + tests).
- New `guest/resolver/` directory (two `.c` files + one `.h`).
- New `tests/resolve_conformance.rs` (6 integration cases).
- New `tests/conformance/{getaddrinfo_loopback,getaddrinfo_eai_noname}.c`
  (2 marker-enforced cases).
- New `docs/adr/0007-p2-dns-resolver.md` (this file).
- New dep: `hickory-resolver = { version = "0.26",
  default-features = false, features = ["tokio", "system-config"] }`
  (bumped from 0.25 during commit 1 to clear RUSTSEC-2026-0118 +
  RUSTSEC-2026-0119 in `hickory-proto 0.25.2`).
- HANDOFF.md regen with P2-DNS section + test totals
  (490 → 518).
- Test totals: Rust 384 + 26 = **410**; C 106 + 2 = **108**;
  Grand **518**. (`tests/count_tests.sh` is the source of truth.)

### Lock discipline (carries from ADR 0001 / 0006)

- Acquire `resolver_state` lock; clone `Arc<dyn ResolverBackend>` if
  built; drop lock before `.await`.
- After `.await`, re-acquire to insert into cache and apply denylist.
- Never hold `parking_lot::Mutex` guard or any guest-memory borrow
  across `.await`. `Caller<'_, Kernel>` is `!Send`.

## Deferred (out of scope for v1)

- **UDP socket layer in kernel** (Path A) — separate, larger workstream.
- **`AI_NUMERICHOST` / `AI_NUMERICSERV` hint flags** → `-EAI_BADFLAGS`
  (musl usually handles these in libc anyway).
- **`getservbyname()`** — v1 numeric-only service strings.
- **PTR (reverse DNS).**
- **Per-record denylist** (currently IP-level post-filter).
- **Snapshot persistence of denylist config** — operator re-issues env
  vars on `serve`.
- **`AI_CANONNAME` population** — v1 leaves `ai_canonname = 0`.

## Verification

- `cargo clippy --profile ci --all-targets -- -D warnings` — clean.
- `cargo test --profile ci` — all 410 Rust tests pass (6 new in
  `resolve_conformance`).
- `bash tests/conformance/runner.sh` — 108/108 (was 106/106).
- `bash scripts/reproduce_dod.sh` — full 8-step sequence green.
- `cargo-deny check` — clean (RUSTSEC-2026-0118/0119 cleared by
  hickory 0.26 bump).

With CPython (when the submodule is available):

- `bash guest/build.sh` produces `python.wasm`; `nm` confirms
  `getaddrinfo`/`freeaddrinfo` resolve to our overrides.
- `cargo run --profile ci --bin edge-cli -- run target/python.wasm -- -c "import socket; print(socket.getaddrinfo('localhost', 80))"`
  prints a list of 5-tuples including `('127.0.0.1', 80)`.
- With `EDGE_RESOLVER_DENY=127.0.0.1`: `socket.gaierror`.